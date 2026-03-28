use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::api::AppState;
use crate::config::ProbeConfig;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ServiceState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, collect_pending_probes, emit_svc_display_states,
    emit_target_states,
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

    // Mark target + transitive dependency targets as activated
    {
        let mut targets = state.targets.write().await;
        super::activate_target_transitive(&mut targets, target_name);
    }

    state.events.emit(Event::op_start("converge", target_name));

    let needed_services: Vec<String> = {
        let mut svcs: Vec<String> = transitive_probes
            .iter()
            .map(|cr| cr.service.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let graph = &state.graph;
        let topo = &graph.topo_order;
        svcs.sort_by_key(|s| topo.iter().position(|t| t == s).unwrap_or(usize::MAX));
        svcs
    };

    // Emit initial display states for all services + targets after target activation.
    // This ensures stopped services now show red (needed by active target).
    {
        emit_svc_display_states(state).await;
        let needed_refs: Vec<&str> = needed_services.iter().map(|s| s.as_str()).collect();
        emit_target_states(state, &needed_refs).await;
    }

    // Single outer timeout wrapping the entire converge loop.
    let loop_result = tokio::time::timeout(
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
    .await;
    let timed_out = loop_result.is_err();
    let final_outcome = loop_result.unwrap_or(PipelineOutcome::NoProgress);

    if timed_out {
        tracing::warn!("cmd converge: timed out after {timeout:?}");
    }

    // Compute result from current state
    let (result_str, target_statuses) = {
        emit_svc_display_states(state).await;
        let affected_svcs: Vec<String> = transitive_probes
            .iter()
            .map(|c| c.service.clone())
            .collect();
        let affected_refs: Vec<&str> = affected_svcs.iter().map(|s| s.as_str()).collect();
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

    // Compute not_green: services in target scope that aren't fully green
    let not_green = if result_str == "failed" {
        let services = state.services.read().await;
        let mut ng = Vec::new();
        let mut seen = HashSet::new();
        for pr in &transitive_probes {
            if !seen.insert(pr.service.clone()) {
                continue;
            }
            if let Some(svc) = services.get(&pr.service) {
                let all_green = svc.probes.values().all(|p| p.state.is_green());
                if !all_green || !matches!(svc.state, ServiceState::Running) {
                    ng.push(pr.service.clone());
                }
            }
        }
        ng
    } else {
        vec![]
    };

    // Build concise error string
    let error = if result_str == "failed" {
        let reason = match (timed_out, final_outcome) {
            (true, _) => "timeout",
            (_, PipelineOutcome::TerminalFailure) => "restart_on_fail=false service has red probes",
            (_, PipelineOutcome::NoProgress) => "no progress possible",
            _ => "target not ready",
        };
        if actions.start_errors.is_empty() {
            Some(reason.to_string())
        } else {
            let failed: Vec<String> = actions.start_errors.keys().cloned().collect();
            Some(format!("{reason}; failed to start: {}", failed.join(", ")))
        }
    } else {
        None
    };

    Ok(OpResponse {
        result: result_str.to_string(),
        duration_ms,
        error,
        not_green,
        actions,
        probes: probe_statuses,
        targets: target_statuses,
    })
}

/// Result of a single item completing in the pipeline.
enum PipelineItem {
    Started {
        svc_name: String,
        result: Box<crate::error::Result<crate::ops::OpResponse>>,
    },
    Probed {
        probe_ref: ProbeRef,
        svc_name: String,
        outcome: crate::probe::ProbeOutcome,
    },
}

/// Inner loop — cancellation-safe because tokio::time::timeout drops this future on timeout.
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
) -> PipelineOutcome {
    let start_time = Instant::now();
    // Shared notification: signaled whenever probe state changes.
    // Dep-waiters subscribe and wake immediately instead of polling.
    let notify = Arc::new(tokio::sync::Notify::new());

    loop {
        let remaining = timeout.saturating_sub(start_time.elapsed());
        if remaining.is_zero() {
            return PipelineOutcome::NoProgress;
        }

        // Quick refresh: reprobe pending probes on already-running services
        {
            let pending = collect_pending_probes(state, Some(transitive_probes)).await;
            if !pending.is_empty() {
                let remaining = timeout.saturating_sub(start_time.elapsed());
                super::probe_and_resolve(state, &pending, probe_statuses, remaining).await;
                notify.notify_waiters();
            }
        }

        if all_green(state, transitive_probes).await {
            return PipelineOutcome::AllGreen;
        }

        let outcome = run_pipeline(
            state,
            needed_services,
            transitive_probes,
            allow_restart,
            timeout.saturating_sub(start_time.elapsed()),
            &notify,
            actions,
            probe_statuses,
            restarted,
        )
        .await;

        match outcome {
            PipelineOutcome::Restarted => continue,
            other => return other,
        }
    }
}

#[derive(Clone, Copy)]
enum PipelineOutcome {
    AllGreen,
    Restarted,
    TerminalFailure,
    NoProgress,
}

/// Concurrent start+probe pipeline. Services start as soon as their deps are green
/// (notified, not polled). Probes fire immediately after start. Results are processed
/// as they arrive, unblocking dependent services instantly.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    state: &Arc<AppState>,
    needed_services: &[String],
    transitive_probes: &[ProbeRef],
    allow_restart: bool,
    remaining: Duration,
    notify: &Arc<tokio::sync::Notify>,
    actions: &mut OpActions,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    restarted: &mut HashSet<String>,
) -> PipelineOutcome {
    let deadline = Instant::now() + remaining;
    let backoff = state.config.defaults.probe_backoff.clone();
    let docker = state.docker.inner();

    let mut futs: futures::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = PipelineItem> + Send>>,
    > = futures::stream::FuturesUnordered::new();

    // Collect services that need starting and probes that need running
    let (to_start, initial_probes) = {
        let services = state.services.read().await;
        let mut starts = Vec::new();
        let mut probes = Vec::new();

        for svc_name in needed_services {
            let svc = &services[svc_name];
            if svc.state != ServiceState::Running {
                // Service needs starting — push a start task
                let deps = svc.start_after.clone();
                starts.push((svc_name.clone(), deps));
            } else {
                // Service already running — push probe tasks for non-green probes
                for (probe_name, probe_rt) in &svc.probes {
                    if probe_rt.state.is_green() {
                        continue;
                    }
                    if probe_rt.is_meta() {
                        continue;
                    }
                    probes.push((
                        ProbeRef::new(svc_name, probe_name),
                        svc_name.clone(),
                        svc.container.clone(),
                        probe_rt.probe_config.clone(),
                        probe_rt.depends_on.clone(),
                        svc.log_since,
                        svc.generation,
                    ));
                }
            }
        }
        (starts, probes)
    };

    // Launch start tasks — each waits for deps via Notify, then starts the service
    for (svc_name, start_after_deps) in to_start {
        let state = state.clone();
        let notify = notify.clone();
        futs.push(Box::pin(async move {
            // Wait for start_after deps to be green (no polling!)
            let deps_ok = wait_for_deps_green(&state, &start_after_deps, &notify, deadline).await;
            if !deps_ok {
                return PipelineItem::Started {
                    svc_name: svc_name.clone(),
                    result: Box::new(Err(GantryError::Operation(format!(
                        "start_after deps not satisfied for {svc_name}"
                    )))),
                };
            }
            if !start_after_deps.is_empty() {
                let deps: Vec<String> = start_after_deps.iter().map(|d| d.to_string()).collect();
                tracing::debug!("svc [{svc_name}] waiting for {}", deps.join(", "));
            }
            // Compute remaining time now (after waiting for deps)
            let actual_remaining = deadline.saturating_duration_since(Instant::now());
            let result = super::start::start(&state, &svc_name, actual_remaining, false).await;
            PipelineItem::Started {
                svc_name,
                result: Box::new(result),
            }
        }));
    }

    // Launch probe tasks for already-running services
    for (probe_ref, svc_name, container, probe_config, deps, log_since, probe_gen) in initial_probes
    {
        push_probe_task(
            &mut futs,
            state,
            docker,
            notify,
            &backoff,
            probe_ref,
            svc_name,
            container,
            probe_config,
            deps,
            log_since,
            probe_gen,
            deadline,
        );
    }

    if futs.is_empty() {
        return if all_green(state, transitive_probes).await {
            PipelineOutcome::AllGreen
        } else {
            PipelineOutcome::NoProgress
        };
    }

    // Process results as they arrive
    let just_started_in_this_pipeline: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
    let mut did_restart = false;

    while let Some(item) = futs.next().await {
        match item {
            PipelineItem::Started { svc_name, result } => match *result {
                Ok(resp) => {
                    for (key, value) in &resp.probes {
                        probe_statuses.insert(key.clone(), value.clone());
                    }
                    actions.started.push(svc_name.clone());
                    just_started_in_this_pipeline
                        .borrow_mut()
                        .insert(svc_name.clone());
                    // Notify all — this service's probes may now be green,
                    // unblocking other services' start_after deps.
                    notify.notify_waiters();

                    // Push probe tasks for any non-green probes on this service.
                    // The start operation already probed, but some might still be red/pending.
                    let extra_probes = {
                        let services = state.services.read().await;
                        let svc = &services[&svc_name];
                        svc.probes
                            .iter()
                            .filter(|(_, p)| !p.state.is_green() && !p.is_meta())
                            .map(|(pn, p)| {
                                (
                                    ProbeRef::new(&svc_name, pn),
                                    svc_name.clone(),
                                    svc.container.clone(),
                                    p.probe_config.clone(),
                                    p.depends_on.clone(),
                                    svc.log_since,
                                    svc.generation,
                                )
                            })
                            .collect::<Vec<_>>()
                    };
                    for (pr, sn, ctr, pc, deps, ls, pg) in extra_probes {
                        push_probe_task(
                            &mut futs, state, docker, notify, &backoff, pr, sn, ctr, pc, deps, ls,
                            pg, deadline,
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("cmd converge: failed to start {svc_name}: {e}");
                    actions.start_errors.insert(svc_name.clone(), e.to_string());
                    // Mark probes as ProbeFailed so dependent wait_for_deps_green bails
                    {
                        let mut services = state.services.write().await;
                        if let Some(svc) = services.get_mut(&svc_name) {
                            for probe in svc.probes.values_mut() {
                                probe.state = crate::model::ProbeState::Red(
                                    crate::model::RedReason::ProbeFailed(
                                        crate::model::ProbeFailure {
                                            error: e.to_string(),
                                            duration_ms: 0,
                                        },
                                    ),
                                );
                            }
                        }
                    }
                    // Notify waiters so tasks depending on this service can bail early
                    notify.notify_waiters();
                }
            },
            PipelineItem::Probed {
                probe_ref,
                svc_name,
                outcome,
            } => {
                let mut affected =
                    crate::ops::apply_probe_result(state, &probe_ref, &outcome, probe_statuses)
                        .await;
                // Update meta probes for this service
                crate::ops::update_meta_probes(state, &svc_name, probe_statuses).await;
                affected.insert(svc_name.clone());
                // Emit display states for probed service + propagation-affected services
                emit_svc_display_states(state).await;
                // Notify all — this probe result may unblock dependents
                notify.notify_waiters();

                // Check restart condition
                let probe_failed = !outcome.result.is_ok();
                if probe_failed
                    && allow_restart
                    && !restarted.contains(&svc_name)
                    && !just_started_in_this_pipeline.borrow().contains(&svc_name)
                {
                    let should_restart = {
                        let services = state.services.read().await;
                        if let Some(svc) = services.get(&svc_name) {
                            svc.restart_on_fail
                                && svc.state == ServiceState::Running
                                && svc.probes.values().any(|p| p.state.is_red())
                        } else {
                            false
                        }
                    };
                    if should_restart {
                        state.events.emit(Event::op_start("restart", &svc_name));
                        if let Err(e) = super::stop::stop(state, &svc_name).await {
                            tracing::warn!("cmd restart: failed to stop {svc_name}: {e}");
                        } else {
                            // No generation bump here — stop() and start() each bump it.
                            actions.restarted.push(svc_name.clone());
                            restarted.insert(svc_name);
                            did_restart = true;
                            break; // Exit pipeline, loop back to start this service
                        }
                    }
                }
            }
        }

        // Early completion check — if all green, stop processing
        if all_green(state, transitive_probes).await {
            return PipelineOutcome::AllGreen;
        }
    }

    if did_restart {
        return PipelineOutcome::Restarted;
    }

    // Check terminal failure after all futures are exhausted (not mid-stream,
    // because a probe might fail temporarily then succeed on retry).
    // 1. restart_on_fail=false service with ProbeFailed (own failure, not dep)
    // 2. skip_restart mode with ProbeFailed
    // 3. A service failed to start (recorded in start_errors)
    let has_terminal = {
        let services = state.services.read().await;
        !actions.start_errors.is_empty()
            || needed_services.iter().any(|svc_name| {
                let svc = &services[svc_name];
                let has_probe_failed = svc.probes.values().any(|p| p.state.is_probe_failed());
                // Terminal if probe failed and either can't restart or not allowed to
                (!allow_restart || !svc.restart_on_fail)
                    && svc.state == ServiceState::Running
                    && has_probe_failed
            })
    };
    if has_terminal {
        tracing::info!("cmd converge: terminal failure detected");
        return PipelineOutcome::TerminalFailure;
    }

    PipelineOutcome::NoProgress
}

/// Push a probe task into the FuturesUnordered. The task waits for probe deps
/// to be green (via Notify), then runs the probe with retry.
#[allow(clippy::too_many_arguments)]
fn push_probe_task(
    futs: &mut futures::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = PipelineItem> + Send>>,
    >,
    state: &Arc<AppState>,
    docker: &bollard::Docker,
    notify: &Arc<tokio::sync::Notify>,
    backoff: &crate::config::BackoffConfig,
    probe_ref: ProbeRef,
    svc_name: String,
    container: String,
    probe_config: ProbeConfig,
    deps: Vec<ProbeRef>,
    log_since: i64,
    generation: u64,
    deadline: Instant,
) {
    let state = state.clone();
    let docker = docker.clone();
    let notify = notify.clone();
    let backoff = backoff.clone();
    futs.push(Box::pin(async move {
        // Wait for probe deps to be green (no polling!)
        if !deps.is_empty() {
            let deps_ok = wait_for_deps_green(&state, &deps, &notify, deadline).await;
            if !deps_ok {
                // Deps didn't go green in time — return a timeout failure
                let mut outcome =
                    crate::probe::ProbeOutcome::immediate(crate::probe::ProbeResult::Failed {
                        error: "probe deps not green in time".into(),
                        duration_ms: 0,
                    });
                outcome.generation = generation;
                return PipelineItem::Probed {
                    probe_ref: probe_ref.clone(),
                    svc_name,
                    outcome,
                };
            }
        }
        tracing::debug!("prb [{svc_name}.{}] probing", probe_ref.probe);
        // Compute remaining time now (after waiting for deps) instead of using
        // the stale `remaining` captured at pipeline start.
        let actual_remaining = deadline.saturating_duration_since(Instant::now());
        let mut result = crate::probe::run_with_retry(
            &docker,
            &svc_name,
            &container,
            &probe_config,
            actual_remaining,
            &backoff,
            log_since,
        )
        .await;
        result.generation = generation;
        PipelineItem::Probed {
            probe_ref,
            svc_name,
            outcome: result,
        }
    }));
}

/// Wait for all deps to be green, using Notify for instant wake-up.
/// Returns false on timeout or if any dep is terminally Red (ProbeFailed).
async fn wait_for_deps_green(
    state: &AppState,
    deps: &[ProbeRef],
    notify: &tokio::sync::Notify,
    deadline: Instant,
) -> bool {
    loop {
        // Register interest BEFORE checking — avoids missing notifications
        let notified = notify.notified();

        // Check if all deps are green, or if any dep is terminally failed
        let (all_green, any_terminal) = {
            let services = state.services.read().await;
            let green = deps.iter().all(|dep| {
                services
                    .get(&dep.service)
                    .and_then(|s| s.probes.get(&dep.probe))
                    .is_some_and(|p| p.state.is_green())
            });
            // Bail early if any dep probe failed (own failure, not recoverable
            // without restart). Start failures also land here — failed services'
            // probes are marked ProbeFailed by the pipeline.
            let terminal = deps.iter().any(|dep| {
                services.get(&dep.service).is_some_and(|s| {
                    s.probes
                        .get(&dep.probe)
                        .is_some_and(|p| p.state.is_probe_failed())
                })
            });
            (green, terminal)
        };
        if all_green {
            return true;
        }
        if any_terminal {
            return false;
        }

        if Instant::now() >= deadline {
            return false;
        }

        // Wait for notification or deadline — no polling!
        tokio::select! {
            _ = notified => {} // Something changed, re-check
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return false;
            }
        }
    }
}

async fn all_green(state: &AppState, transitive_probes: &[ProbeRef]) -> bool {
    let services = state.services.read().await;
    transitive_probes.iter().all(|pr| {
        services
            .get(&pr.service)
            .and_then(|s| s.probes.get(&pr.probe))
            .is_some_and(|p| p.state.is_green())
    })
}
