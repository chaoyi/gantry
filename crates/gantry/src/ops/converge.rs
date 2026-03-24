use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ServiceState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, collect_stale_or_red_probes, collect_stale_probes,
    emit_svc_display_states, emit_target_states,
};

pub async fn converge(
    state: &Arc<AppState>,
    target_name: &str,
    timeout: Duration,
    allow_restart: bool,
) -> Result<OpResponse> {
    let start_time = Instant::now();
    let mut actions = OpActions::default();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    let mut restarted: HashSet<String> = HashSet::new();

    let transitive_probes;
    {
        let targets = state.targets.read().await;
        let tgt = targets
            .get(target_name)
            .ok_or_else(|| GantryError::NotFound(format!("target '{target_name}' not found")))?;
        transitive_probes = tgt.transitive_probes.clone();
    }

    state.events.emit(Event::op_start("converge", target_name));

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

    // Single outer timeout wrapping the entire converge loop.
    // On timeout, all in-flight work stops and we report current state.
    let timed_out = tokio::time::timeout(
        timeout,
        converge_loop(
            state,
            &needed_services,
            &transitive_probes,
            allow_restart,
            timeout,
            &mut actions,
            &mut probe_statuses,
            &mut restarted,
        ),
    )
    .await
    .is_err();

    if timed_out {
        tracing::warn!("converge: timed out after {timeout:?}");
    }

    // Compute result from current state
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
            let mut parts = Vec::new();
            if timed_out {
                parts.push("timeout".to_string());
            }
            if !actions.start_errors.is_empty() {
                let details: Vec<String> = actions
                    .start_errors
                    .iter()
                    .map(|(svc, err)| format!("{svc}: {err}"))
                    .collect();
                parts.push(format!("failed to start: {}", details.join("; ")));
            }
            if parts.is_empty() {
                parts.push("target not ready".to_string());
            }
            Some(parts.join("; "))
        } else {
            None
        },
        actions,
        probes: probe_statuses,
        targets: target_statuses,
    })
}

/// Inner loop — cancellation-safe because tokio::time::timeout drops this future on timeout.
/// All spawned tasks use the remaining deadline, so they also stop promptly.
#[allow(clippy::too_many_arguments)]
async fn converge_loop(
    state: &Arc<AppState>,
    needed_services: &[String],
    transitive_probes: &[ProbeRef],
    allow_restart: bool,
    timeout: Duration,
    actions: &mut OpActions,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    restarted: &mut HashSet<String>,
) {
    let start_time = Instant::now();

    loop {
        let remaining = timeout.saturating_sub(start_time.elapsed());
        if remaining.is_zero() {
            break;
        }
        let started_before = actions.started.len();
        let restarted_before = actions.restarted.len();

        // Step 1: Refresh — single-probe stale probes on already-running services so that
        // start_after deps can be evaluated (e.g. after external docker restart).
        {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            let stale = collect_stale_probes(state, Some(transitive_probes)).await;
            if !stale.is_empty() {
                super::probe_and_resolve(state, &stale, probe_statuses, remaining).await;
            }
        }

        // Step 2: Start stopped services (cascade via start_after, parallel)
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

        if !to_start.is_empty() {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            // Shared set: when a start fails, other futures waiting on that
            // service's probes can bail early instead of polling until timeout.
            let failed_starts: Arc<tokio::sync::Mutex<HashSet<String>>> =
                Arc::new(tokio::sync::Mutex::new(HashSet::new()));

            let mut futs = futures::stream::FuturesUnordered::new();
            for (svc_name, start_after_deps) in to_start {
                let state = state.clone();
                let failed_starts = failed_starts.clone();
                futs.push(async move {
                    let deadline = Instant::now() + remaining;
                    let mut unmet_dep: Option<String> = None;
                    for dep in &start_after_deps {
                        let mut met = false;
                        while Instant::now() < deadline {
                            let services = state.services.read().await;
                            if let Some(svc) = services.get(&dep.service)
                                && let Some(probe) = svc.probes.get(&dep.probe)
                                && probe.state.is_green()
                            {
                                met = true;
                                break;
                            }
                            drop(services);
                            // Bail early if dep service failed to start
                            if failed_starts.lock().await.contains(&dep.service) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        if !met {
                            unmet_dep = Some(dep.to_string());
                            break;
                        }
                    }
                    if let Some(dep) = unmet_dep {
                        return (
                            svc_name,
                            Err(crate::error::GantryError::Operation(format!(
                                "dependency {dep} not satisfied"
                            ))),
                        );
                    }
                    if start_after_deps.is_empty() {
                        tracing::info!("[{svc_name}] starting");
                    } else {
                        let deps: Vec<String> =
                            start_after_deps.iter().map(|d| d.to_string()).collect();
                        tracing::info!("[{svc_name}] starting (after {})", deps.join(", "));
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let result = super::start::start(&state, &svc_name, remaining, false).await;
                    (svc_name, result)
                });
            }

            // Process results as they arrive — no spawned tasks to outlive this scope
            while let Some((svc_name, result)) = futs.next().await {
                match result {
                    Ok(resp) => {
                        for (key, value) in &resp.probes {
                            probe_statuses.insert(key.clone(), value.clone());
                        }
                        actions.started.push(svc_name.clone());
                    }
                    Err(e) => {
                        tracing::warn!("converge: failed to start {svc_name}: {e}");
                        failed_starts.lock().await.insert(svc_name.clone());
                        actions.start_errors.insert(svc_name.clone(), e.to_string());
                    }
                }
            }
        }

        let just_started: HashSet<String> =
            actions.started[started_before..].iter().cloned().collect();

        // Step 3: Probe stale/red probes AND restart fast-failing services.
        // Probes run in parallel. As each completes, we check if the service
        // should be restarted — no waiting for slow probes to finish first.
        {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            let stale_probes = collect_stale_or_red_probes(state, Some(transitive_probes)).await;

            if stale_probes.is_empty() {
                break; // nothing to probe, we're done
            }

            // Track which services have all probes resolved
            let mut pending_per_svc: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for (pr, _, _, _) in &stale_probes {
                *pending_per_svc.entry(pr.service.clone()).or_insert(0) += 1;
            }

            // Fire probes — use svc.log_since (not container boot time)
            let docker = state.docker.inner();
            let backoff = state.config.read().await.defaults.probe_backoff.clone();
            let svc_log_since: std::collections::HashMap<String, i64> = {
                let services = state.services.read().await;
                stale_probes
                    .iter()
                    .map(|(pr, _, _, _)| (pr.service.clone(), services[&pr.service].log_since))
                    .collect()
            };

            let mut futs = futures::stream::FuturesUnordered::new();
            for (probe_ref, svc_name, container, probe_config) in &stale_probes {
                let docker = docker.clone();
                let svc = svc_name.clone();
                let ctr = container.clone();
                let pc = probe_config.clone();
                let cr = probe_ref.clone();
                let backoff = backoff.clone();
                let log_since = svc_log_since.get(svc_name).copied().unwrap_or(0);
                tracing::debug!(
                    "[{svc_name}.{}] probe with log_since={log_since}",
                    probe_ref.probe
                );
                futs.push(async move {
                    let result = crate::probe::run_with_retry(
                        &docker, &svc, &ctr, &pc, remaining, &backoff, log_since,
                    )
                    .await;
                    (cr, result)
                });
            }

            // Process results as they arrive — restart fast-failing services immediately
            let mut batch_results = Vec::new();
            let mut did_restart = false;
            while let Some((pr, outcome)) = futs.next().await {
                let probe_failed = !outcome.result.is_ok();
                batch_results.push((pr.clone(), outcome));

                if let Some(count) = pending_per_svc.get_mut(&pr.service) {
                    *count = count.saturating_sub(1);
                    if probe_failed
                        && allow_restart
                        && !restarted.contains(&pr.service)
                        && !just_started.contains(&pr.service)
                    {
                        // Apply results so far to update state
                        if !batch_results.is_empty() {
                            crate::ops::resolve_probe_batch(state, &batch_results, probe_statuses)
                                .await;
                            batch_results.clear();
                        }

                        let should_restart = {
                            let services = state.services.read().await;
                            if let Some(svc) = services.get(&pr.service) {
                                svc.restart_on_fail
                                    && svc.state == ServiceState::Running
                                    && svc.probes.values().any(|p| p.state.is_red())
                            } else {
                                false
                            }
                        };

                        if should_restart {
                            tracing::info!("restart {} (probes red)", pr.service);
                            state.events.emit(Event::ServiceRestart {
                                service: pr.service.clone(),
                                reason: "probes red".into(),
                                ts: chrono::Utc::now().timestamp_millis(),
                            });
                            if let Err(e) = super::stop::stop(state, &pr.service).await {
                                tracing::warn!("converge: failed to stop {}: {e}", pr.service);
                            } else {
                                // Increment generation so stale probe results are discarded
                                {
                                    let mut services = state.services.write().await;
                                    if let Some(svc) = services.get_mut(&pr.service) {
                                        svc.generation += 1;
                                    }
                                }
                                actions.restarted.push(pr.service.clone());
                                restarted.insert(pr.service.clone());
                                did_restart = true;
                                // Break out of probe loop — loop back to step 1
                                // to start the restarted service. Remaining in-flight
                                // probes are dropped (FuturesUnordered cleanup).
                                break;
                            }
                        }
                    }
                }
            }

            // Resolve any remaining batch results
            if !batch_results.is_empty() {
                crate::ops::resolve_probe_batch(state, &batch_results, probe_statuses).await;
            }

            // If we restarted a service, loop back immediately (don't wait for slow probes)
            if did_restart {
                continue;
            }
        }

        // Step 4: Check completion — all green? Done.
        let all_green = {
            let services = state.services.read().await;
            transitive_probes.iter().all(|pr| {
                services
                    .get(&pr.service)
                    .and_then(|s| s.probes.get(&pr.probe))
                    .is_some_and(|p| p.state.is_green())
            })
        };
        if all_green || !allow_restart {
            break;
        }

        // Any services stopped by restart? Loop back to step 1 to start them.
        let has_stopped = {
            let services = state.services.read().await;
            needed_services
                .iter()
                .any(|s| services[s].state != ServiceState::Running)
        };
        if !has_stopped {
            break; // nothing to restart, done
        }

        // No progress? Break to avoid spinning forever (e.g. container can't start).
        let made_progress =
            actions.started.len() > started_before || actions.restarted.len() > restarted_before;
        if !made_progress {
            break;
        }
        // Loop back: step 1 will start the stopped services
    }
}
