pub mod converge;
pub mod reprobe;
pub mod restart;
pub mod start;
pub mod stop;

use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::api::AppState;
use crate::config::ProbeConfig;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ProbeState};

pub struct OpLock {
    inner: Mutex<Option<String>>,
}

impl OpLock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
        })
    }

    pub fn current_op(&self) -> Option<String> {
        self.inner.lock().unwrap().clone()
    }

    pub fn try_acquire(&self, op_name: &str) -> Result<OpGuard<'_>> {
        let mut lock = self.inner.lock().unwrap();
        if let Some(current) = lock.as_ref() {
            return Err(GantryError::Conflict(format!(
                "operation in progress: {current}"
            )));
        }
        *lock = Some(op_name.to_string());
        Ok(OpGuard {
            lock_ref: &self.inner,
        })
    }
}

pub struct OpGuard<'a> {
    lock_ref: &'a Mutex<Option<String>>,
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        // std::sync::Mutex::lock() always succeeds (blocks if needed, never fails)
        *self.lock_ref.lock().unwrap() = None;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OpResponse {
    pub result: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub actions: OpActions,
    pub probes: indexmap::IndexMap<String, ProbeStatus>,
    pub targets: indexmap::IndexMap<String, TargetStatus>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OpActions {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub started: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restarted: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<String>,
    #[serde(skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub start_errors: indexmap::IndexMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeStatus {
    pub state: String,
    pub prev: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    pub state: String,
    pub prev: String,
}

/// Emit probe events + probe_statuses for changes returned by graph propagation methods.
pub fn emit_propagated_changes(
    state: &AppState,
    services: &indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    changes: &[(
        crate::model::ProbeRef,
        crate::model::ProbeState,
        crate::model::ProbeState,
    )],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    use crate::model::{ProbeDisplayState, ServiceState};
    for (cr, new_state, prev) in changes {
        let svc_state = services
            .get(&cr.service)
            .map(|s| s.state)
            .unwrap_or(ServiceState::Stopped);
        let probe_rt = services
            .get(&cr.service)
            .and_then(|s| s.probes.get(&cr.probe));
        let display = if let Some(p) = probe_rt {
            ProbeDisplayState::from_probe(p, svc_state)
        } else {
            ProbeDisplayState::Stopped
        };
        let display_str = display.as_str();
        if new_state.as_str() != prev.as_str() {
            state.events.emit(Event::probe_state_change(
                cr,
                new_state.clone(),
                prev.clone(),
                display_str,
            ));
        }
        probe_statuses.insert(
            cr.to_string(),
            ProbeStatus {
                state: display_str.into(),
                prev: prev.as_str().into(),
                reason: new_state.reason(),
                probe_ms: None,
                error: None,
                logs: None,
            },
        );
    }
}

// ── Shared helpers used by stop, start, converge, reprobe ─────────────

/// Check whether a probe's declared dependencies are all green or any are red.
/// Returns `(deps_all_green, deps_any_red)`.
///
/// Must be called while holding the write lock on `services` so the dependency
/// check and the subsequent state update are atomic (no TOCTOU).
fn check_deps(
    services: &indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    probe_ref: &ProbeRef,
) -> (bool, bool) {
    let deps: &[ProbeRef] = &services[&probe_ref.service].probes[&probe_ref.probe].depends_on;
    let all_green = deps.iter().all(|dep| {
        services
            .get(&dep.service)
            .and_then(|s| s.probes.get(&dep.probe))
            .is_some_and(|c| c.state.is_green())
    });
    let any_red = deps.iter().any(|dep| {
        services
            .get(&dep.service)
            .and_then(|s| s.probes.get(&dep.probe))
            .is_none_or(|c| c.state.is_red())
    });
    (all_green, any_red)
}

/// Handle a successful probe result within the already-locked `services` map.
///
/// Updates the probe's state (accounting for dep satisfaction), emits the
/// state-change event, records the status, and propagates recovery if the
/// probe just turned green.
async fn apply_ok_result(
    state: &AppState,
    probe_ref: &ProbeRef,
    duration_ms: u64,
    deps_all_green: bool,
    deps_any_red: bool,
    services: &mut indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    let deps = &services[&probe_ref.service].probes[&probe_ref.probe].depends_on;
    let (new_state, dep_error) = if deps_all_green {
        (ProbeState::Green, None)
    } else if deps_any_red {
        // Find first red dep for the reason
        let first_red_dep = deps
            .iter()
            .find(|d| {
                services
                    .get(&d.service)
                    .and_then(|s| s.probes.get(&d.probe))
                    .is_none_or(|p| p.state.is_red())
            })
            .cloned()
            .unwrap_or_else(|| probe_ref.clone());
        let red_deps: Vec<String> = deps
            .iter()
            .filter(|d| {
                services
                    .get(&d.service)
                    .and_then(|s| s.probes.get(&d.probe))
                    .is_none_or(|p| p.state.is_red())
            })
            .map(|d| d.to_string())
            .collect();
        (
            ProbeState::Red(crate::model::RedReason::DepRed { dep: first_red_dep }),
            Some(format!("dep red: {}", red_deps.join(", "))),
        )
    } else {
        let first_non_green_dep = deps
            .iter()
            .find(|d| {
                services
                    .get(&d.service)
                    .and_then(|s| s.probes.get(&d.probe))
                    .is_some_and(|p| !p.state.is_green())
            })
            .cloned()
            .unwrap_or_else(|| probe_ref.clone());
        (
            ProbeState::Stale(crate::model::StaleReason::DepNotReady {
                dep: first_non_green_dep,
            }),
            Some("deps not all green".into()),
        )
    };

    let probe = services
        .get_mut(&probe_ref.service)
        .unwrap()
        .probes
        .get_mut(&probe_ref.probe)
        .unwrap();
    let prev = probe.state.clone();
    probe.prev_color = Some(prev.color());
    probe.state = new_state.clone();
    probe.last_probe_ms = Some(duration_ms);
    probe.last_error = dep_error.clone();

    state.events.emit(Event::probe_state_change(
        probe_ref,
        new_state.clone(),
        prev.clone(),
        new_state.as_str(),
    ));
    probe_statuses.insert(
        probe_ref.to_string(),
        ProbeStatus {
            state: new_state.as_str().into(),
            prev: prev.as_str().into(),
            reason: new_state.reason(),
            probe_ms: Some(duration_ms),
            error: dep_error,
            logs: None,
        },
    );

    // Recovery propagation: when a probe goes green, mark red reverse-deps as stale
    // (their dependency recovered, they should be reprobed)
    if new_state.is_green() && !prev.is_green() {
        let graph = state.graph.read().await;
        let mut recovery_changes = Vec::new();
        graph.propagate_recovery(&probe_ref.to_string(), services, &mut recovery_changes);
        emit_propagated_changes(state, services, &recovery_changes, probe_statuses);
    }
}

/// Handle a failed probe result within the already-locked `services` map.
///
/// Sets the probe to red, emits the state-change event, records the status,
/// and propagates staleness/red to dependents if the probe was not already red.
async fn apply_failed_result(
    state: &AppState,
    probe_ref: &ProbeRef,
    error: &str,
    duration_ms: u64,
    services: &mut indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    let new_state = ProbeState::Red(crate::model::RedReason::ProbeFailed(
        crate::model::ProbeFailure {
            error: error.to_string(),
            duration_ms,
        },
    ));
    let probe = services
        .get_mut(&probe_ref.service)
        .unwrap()
        .probes
        .get_mut(&probe_ref.probe)
        .unwrap();
    let prev = probe.state.clone();
    probe.prev_color = Some(prev.color());
    probe.state = new_state.clone();
    probe.last_probe_ms = Some(duration_ms);
    probe.last_error = Some(error.to_string());

    state.events.emit(Event::probe_state_change(
        probe_ref,
        new_state,
        prev.clone(),
        "red",
    ));
    probe_statuses.insert(
        probe_ref.to_string(),
        ProbeStatus {
            state: "red".into(),
            prev: prev.as_str().into(),
            reason: Some(format!("probe failed: {error}")),
            probe_ms: Some(duration_ms),
            error: Some(error.to_string()),
            logs: None,
        },
    );

    // Red propagation: when a probe goes red, propagate to dependents
    if !prev.is_red() {
        let graph = state.graph.read().await;
        let mut red_changes = Vec::new();
        graph.propagate_staleness(&probe_ref.to_string(), services, &mut red_changes);
        emit_propagated_changes(state, services, &red_changes, probe_statuses);
    }
}

/// Apply a probe outcome: emit attempt events, update probe state, record in probe_statuses.
///
/// A single write lock on `services` covers both the dependency check and the
/// state update, preventing any TOCTOU race.
pub async fn apply_probe_result(
    state: &AppState,
    probe_ref: &ProbeRef,
    outcome: &crate::probe::ProbeOutcome,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    // 1. Emit per-attempt probe events (no lock needed).
    let last_idx = outcome.attempts.len().saturating_sub(1);
    for (i, att) in outcome.attempts.iter().enumerate() {
        let mut event = if i == last_idx && !outcome.matched_lines.is_empty() {
            let last_line = outcome.matched_lines.last().cloned().into_iter().collect();
            Event::probe_result_with_lines(
                probe_ref,
                att.ok,
                Some(att.elapsed_ms),
                att.attempt,
                att.error.clone(),
                last_line,
                att.ts,
            )
        } else {
            Event::probe_result(
                probe_ref,
                att.ok,
                Some(att.elapsed_ms),
                att.attempt,
                att.error.clone(),
                att.ts,
            )
        };
        // Set probe type detail (e.g. "tcp :8080", "log \"ready\"")
        if let Event::ProbeResult {
            ref mut probe_detail,
            ..
        } = event
        {
            *probe_detail = Some(att.detail.clone());
        }
        state.events.emit(event);
    }

    let mut services = state.services.write().await;

    // 2. Check dependency satisfaction (inside the lock — atomic with step 3).
    let (deps_all_green, deps_any_red) = check_deps(&services, probe_ref);

    // 3. Update probe state + emit state-change event + trigger propagation.
    match &outcome.result {
        crate::probe::ProbeResult::Ok { duration_ms } => {
            apply_ok_result(
                state,
                probe_ref,
                *duration_ms,
                deps_all_green,
                deps_any_red,
                &mut services,
                probe_statuses,
            )
            .await;
        }
        crate::probe::ProbeResult::Failed { error, duration_ms } => {
            apply_failed_result(
                state,
                probe_ref,
                error,
                *duration_ms,
                &mut services,
                probe_statuses,
            )
            .await;
        }
    }

    // 4. Attach last matched log line to the probe status (the one that determined the result).
    if !outcome.matched_lines.is_empty()
        && let Some(status) = probe_statuses.get_mut(&probe_ref.to_string())
    {
        status.logs = outcome.matched_lines.last().map(|l| vec![l.clone()]);
    }
}

/// Recompute meta probes for a service based on depends_on satisfaction.
pub async fn update_meta_probes(
    state: &AppState,
    service_name: &str,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    let mut services = state.services.write().await;
    let meta_probes: Vec<String> = {
        let Some(svc) = services.get(service_name) else {
            return;
        };
        svc.probes
            .iter()
            .filter(|(_, probe)| matches!(probe.probe_config, ProbeConfig::Meta))
            .map(|(name, _)| name.clone())
            .collect()
    };
    for probe_name in meta_probes {
        let probe_ref = ProbeRef::new(service_name, &probe_name);
        let satisfied = crate::probe::meta::is_satisfied(&probe_ref, &services);
        let new_state = if satisfied {
            ProbeState::Green
        } else {
            // Find the first unsatisfied dep for the reason
            let deps = &services[service_name].probes[&probe_name].depends_on;
            let first_unsatisfied = deps
                .iter()
                .find(|dep| {
                    !services
                        .get(&dep.service)
                        .and_then(|s| s.probes.get(&dep.probe))
                        .is_some_and(|p| p.state.is_green())
                })
                .cloned()
                .unwrap_or_else(|| probe_ref.clone());
            ProbeState::Red(crate::model::RedReason::DepRed {
                dep: first_unsatisfied,
            })
        };
        let svc = services.get_mut(service_name).unwrap();
        let probe = svc.probes.get_mut(&probe_name).unwrap();
        let prev = probe.state.clone();
        probe.prev_color = Some(prev.color());
        probe.state = new_state.clone();
        if new_state.as_str() != prev.as_str() {
            state.events.emit(Event::probe_state_change(
                &probe_ref,
                new_state.clone(),
                prev.clone(),
                new_state.as_str(),
            ));
        }
        probe_statuses.insert(
            probe_ref.to_string(),
            ProbeStatus {
                state: new_state.as_str().into(),
                prev: prev.as_str().into(),
                reason: new_state.reason(),
                probe_ms: None,
                error: None,
                logs: None,
            },
        );
    }
}

/// Apply a batch of probe results in probe dependency order (petgraph toposort).
/// Each probe's deps are guaranteed to be resolved before the probe itself.
/// Meta probes are resolved inline (after all their deps in the same service).
pub async fn resolve_probe_batch(
    state: &AppState,
    batch: &[(ProbeRef, crate::probe::ProbeOutcome)],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) -> std::collections::HashSet<String> {
    use std::collections::HashMap;

    // Index results by probe key for O(1) lookup
    let mut result_map: HashMap<String, &crate::probe::ProbeOutcome> = HashMap::new();
    for (probe_ref, outcome) in batch {
        result_map.insert(probe_ref.to_string(), outcome);
    }

    // Get probe-level topo order from petgraph
    let probe_topo_order = state.graph.read().await.probe_topo_order();

    let mut affected = std::collections::HashSet::new();
    let mut meta_updated = std::collections::HashSet::new();

    // Apply in probe topo order — deps resolve before dependents
    for probe_key in &probe_topo_order {
        if let Some(outcome) = result_map.get(probe_key) {
            let probe_ref = crate::model::ProbeRef::parse(probe_key).unwrap();
            affected.insert(probe_ref.service.clone());
            apply_probe_result(state, &probe_ref, outcome, probe_statuses).await;

            // After applying a non-meta probe, update meta probes for its service
            // (meta probes aren't probed — they're resolved from their deps)
            if !meta_updated.contains(&probe_ref.service) {
                update_meta_probes(state, &probe_ref.service, probe_statuses).await;
                meta_updated.insert(probe_ref.service.clone());
            }
        }
    }

    // Final meta pass: ensure all affected services have up-to-date meta probes
    // (handles case where multiple probes in same service — update after all are applied)
    meta_updated.clear();
    for svc in &affected {
        update_meta_probes(state, svc, probe_statuses).await;
    }

    let refs: Vec<&str> = affected.iter().map(|s| s.as_str()).collect();
    emit_svc_display_states(state, &refs).await;
    affected
}

/// Collect stale or red non-meta probes. Used by converge where both need reprobing.
/// Always skips probes for stopped services — they can't be probed.
pub async fn collect_stale_or_red_probes(
    state: &AppState,
    scope: Option<&[ProbeRef]>,
) -> Vec<(ProbeRef, String, String, ProbeConfig)> {
    use crate::model::ServiceState;
    let services = state.services.read().await;
    let mut result = Vec::new();

    let mut try_add = |pr: ProbeRef, svc: &crate::model::ServiceRuntime| {
        if svc.state == ServiceState::Stopped {
            return;
        }
        if let Some(probe) = svc.probes.get(&pr.probe)
            && (probe.state.is_stale() || probe.state.is_red())
            && !matches!(probe.probe_config, ProbeConfig::Meta)
        {
            result.push((
                pr,
                svc.name.clone(),
                svc.container.clone(),
                probe.probe_config.clone(),
            ));
        }
    };

    match scope {
        Some(refs) => {
            for cr in refs {
                if let Some(svc) = services.get(&cr.service) {
                    try_add(cr.clone(), svc);
                }
            }
        }
        None => {
            for (svc_name, svc) in services.iter() {
                for probe_name in svc.probes.keys() {
                    try_add(ProbeRef::new(svc_name, probe_name), svc);
                }
            }
        }
    }
    result
}

/// Collect stale non-meta probes. Optionally scoped to specific probe refs.
pub async fn collect_stale_probes(
    state: &AppState,
    scope: Option<&[ProbeRef]>,
) -> Vec<(ProbeRef, String, String, ProbeConfig)> {
    use crate::model::ServiceState;
    let services = state.services.read().await;
    let mut result = Vec::new();

    let mut try_add = |pr: ProbeRef, svc: &crate::model::ServiceRuntime| {
        if let Some(probe) = svc.probes.get(&pr.probe)
            && probe.state.is_stale()
            && !matches!(probe.probe_config, ProbeConfig::Meta)
        {
            result.push((
                pr,
                svc.name.clone(),
                svc.container.clone(),
                probe.probe_config.clone(),
            ));
        }
    };

    match scope {
        Some(refs) => {
            for cr in refs {
                if let Some(svc) = services.get(&cr.service) {
                    try_add(cr.clone(), svc);
                }
            }
        }
        None => {
            for (svc_name, svc) in services.iter() {
                if svc.state == ServiceState::Stopped {
                    continue;
                }
                for probe_name in svc.probes.keys() {
                    try_add(ProbeRef::new(svc_name, probe_name), svc);
                }
            }
        }
    }
    result
}

/// Probe stale probes in parallel with retry+backoff (long probe), then resolve in topo order.
/// Used by converge phases where services may need time to become ready.
pub async fn probe_and_resolve_with_retry(
    state: &AppState,
    stale_probes: &[(ProbeRef, String, String, ProbeConfig)],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    timeout: std::time::Duration,
) {
    if stale_probes.is_empty() {
        return;
    }
    let docker = state.docker.inner();
    let backoff = state.config.read().await.defaults.probe_backoff.clone();

    // Get log_since from service state (tracks last probe time, not container boot time)
    let svc_log_since: std::collections::HashMap<String, i64> = {
        let services = state.services.read().await;
        stale_probes
            .iter()
            .map(|(pr, _, _, _)| (pr.service.clone(), services[&pr.service].log_since))
            .collect()
    };

    let probe_futures: Vec<_> = stale_probes
        .iter()
        .map(|(probe_ref, svc_name, container, probe_config)| {
            let docker = docker.clone();
            let svc = svc_name.clone();
            let ctr = container.clone();
            let pc = probe_config.clone();
            let cr = probe_ref.clone();
            let backoff = backoff.clone();
            let log_since = svc_log_since.get(svc_name).copied().unwrap_or(0);
            async move {
                let result = crate::probe::run_with_retry(
                    &docker, &svc, &ctr, &pc, timeout, &backoff, log_since,
                )
                .await;
                (cr, result)
            }
        })
        .collect();
    let results = futures::future::join_all(probe_futures).await;
    resolve_probe_batch(state, &results, probe_statuses).await;
}

/// Probe stale probes in parallel using single-attempt probes, then resolve in topo order.
/// Used by reprobe operations (quick state check on already-running services).
/// Skips probes whose deps are Red or Stale — no wasted work.
pub async fn probe_and_resolve(
    state: &AppState,
    stale_probes: &[(ProbeRef, String, String, ProbeConfig)],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    timeout: std::time::Duration,
) {
    if stale_probes.is_empty() {
        return;
    }

    // Partition: only probe when all deps are Green; skip if any dep Red/Stale
    let (to_probe, to_skip): (Vec<_>, Vec<_>) = {
        let services = state.services.read().await;
        stale_probes.iter().partition(|(probe_ref, _, _, _)| {
            let deps = &services[&probe_ref.service].probes[&probe_ref.probe].depends_on;
            deps.iter().all(|dep| {
                services
                    .get(&dep.service)
                    .and_then(|s| s.probes.get(&dep.probe))
                    .is_some_and(|p| p.state.is_green())
            })
        })
    };

    // Mark skipped probes with dep-aware state
    if !to_skip.is_empty() {
        let mut services = state.services.write().await;
        for (probe_ref, _, _, _) in &to_skip {
            let deps = &services[&probe_ref.service].probes[&probe_ref.probe].depends_on;
            let first_red_dep = deps
                .iter()
                .find(|dep| {
                    services
                        .get(&dep.service)
                        .and_then(|s| s.probes.get(&dep.probe))
                        .is_none_or(|p| p.state.is_red())
                })
                .cloned();
            let any_red = first_red_dep.is_some();
            let probe = services
                .get_mut(&probe_ref.service)
                .unwrap()
                .probes
                .get_mut(&probe_ref.probe)
                .unwrap();
            let prev = probe.state.clone();
            if let Some(red_dep) = first_red_dep {
                probe.state = ProbeState::Red(crate::model::RedReason::DepRed { dep: red_dep });
            }
            // else: stays Stale (dep is Stale)
            probe.prev_color = Some(prev.color());
            probe_statuses.insert(
                probe_ref.to_string(),
                ProbeStatus {
                    state: probe.state.as_str().into(),
                    prev: prev.as_str().into(),
                    reason: probe.state.reason(),
                    probe_ms: None,
                    error: if any_red {
                        Some("dep red".into())
                    } else {
                        Some("dep not ready".into())
                    },
                    logs: None,
                },
            );
        }
    }

    // Probe the rest
    let docker = state.docker.inner();
    let probe_futures: Vec<_> = to_probe
        .iter()
        .map(|(probe_ref, svc_name, container, probe_config)| {
            let docker = docker.clone();
            let svc = svc_name.clone();
            let ctr = container.clone();
            let pc = probe_config.clone();
            let cr = probe_ref.clone();
            async move {
                let result =
                    crate::probe::run_single_attempt(&docker, &svc, &ctr, &pc, timeout).await;
                (cr, result)
            }
        })
        .collect();
    let results = futures::future::join_all(probe_futures).await;
    resolve_probe_batch(state, &results, probe_statuses).await;
}

/// Emit service display state events for affected services (only on change).
pub async fn emit_svc_display_states(state: &AppState, affected_services: &[&str]) {
    use crate::model::SvcDisplayState;
    let mut services = state.services.write().await;
    for svc_name in affected_services {
        if let Some(svc) = services.get_mut(*svc_name) {
            let display = SvcDisplayState::from_service(svc);
            if svc.last_emitted_display != Some(display) {
                svc.last_emitted_display = Some(display);
                state
                    .events
                    .emit(Event::service_state(svc_name, svc.state, display.as_str()));
            }
        }
    }
}

/// Compute and emit target states for targets affected by the given services.
/// Returns only affected targets in the REST response. Emits WS events only for state changes.
pub async fn emit_target_states(
    state: &AppState,
    affected_services: &[&str],
) -> indexmap::IndexMap<String, TargetStatus> {
    let services = state.services.read().await;
    let mut targets = state.targets.write().await;

    let mut statuses = indexmap::IndexMap::new();
    for (name, tgt) in targets.iter_mut() {
        // Skip targets not transitively dependent on any affected service
        let affected = affected_services.is_empty()
            || tgt
                .transitive_probes
                .iter()
                .any(|c| affected_services.contains(&c.service.as_str()));
        if !affected {
            continue;
        }

        let current = tgt.state(&services);
        let prev = &tgt.last_emitted_state;
        let changed = prev.as_ref().map(|p| p.as_str()) != Some(current.as_str());

        statuses.insert(
            name.clone(),
            TargetStatus {
                state: current.as_str().into(),
                prev: prev
                    .as_ref()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|| current.as_str().into()),
            },
        );

        if changed {
            tgt.last_emitted_state = Some(current.clone());
            state.events.emit(Event::target_state(name, current, None));
        }
    }
    statuses
}
