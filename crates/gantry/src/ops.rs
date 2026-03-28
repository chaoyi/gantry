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
    /// Services that are not green after the operation. Only set on converge failure.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_green: Vec<String>,
    pub actions: OpActions,
    pub probes: indexmap::IndexMap<String, ProbeStatus>,
    pub targets: indexmap::IndexMap<String, TargetStatus>,
}

impl OpResponse {
    pub fn ok(
        start_time: std::time::Instant,
        actions: OpActions,
        probes: indexmap::IndexMap<String, ProbeStatus>,
        targets: indexmap::IndexMap<String, TargetStatus>,
    ) -> Self {
        Self {
            result: "ok".to_string(),
            duration_ms: start_time.elapsed().as_millis() as u64,
            error: None,
            not_green: vec![],
            actions,
            probes,
            targets,
        }
    }
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
        // Emit when anything changed. Skip stopped service probes (always "stopped").
        // Compare by reason string to catch sub-state changes like Pending(ContainerStarted) → Pending(Reprobing).
        let changed = new_state.as_str() != prev.as_str() || new_state.reason() != prev.reason();
        if changed && !matches!(display, ProbeDisplayState::Stopped) {
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

/// Mark all probes in a service as red with the given reason, collecting state changes.
/// Used by stop (Stopped) and watcher handle_die (ContainerDied).
pub fn mark_all_probes_red(
    svc_name: &str,
    svc: &mut crate::model::ServiceRuntime,
    red_reason_fn: impl Fn() -> crate::model::RedReason,
) -> Vec<(ProbeRef, ProbeState, ProbeState)> {
    let mut changes = Vec::new();
    for (probe_name, probe) in svc.probes.iter_mut() {
        let prev = probe.state.clone();
        probe.prev_color = Some(prev.color());
        let new_state = ProbeState::Red(red_reason_fn());
        probe.state = new_state.clone();
        changes.push((ProbeRef::new(svc_name, probe_name), new_state, prev));
    }
    changes
}

/// Mark all probes in a service as pending, collecting state changes.
/// Skips probes already in Pending state.
/// Used by watcher handle_start (ContainerStarted) and reprobe mark_pending_and_propagate (Reprobing).
pub fn mark_all_probes_pending(
    svc_name: &str,
    svc: &mut crate::model::ServiceRuntime,
    reason_fn: impl Fn() -> crate::model::PendingReason,
) -> Vec<(ProbeRef, ProbeState, ProbeState)> {
    let mut changes = Vec::new();
    for (probe_name, probe) in svc.probes.iter_mut() {
        if !probe.state.is_pending() {
            let prev = probe.state.clone();
            probe.prev_color = Some(prev.color());
            let new_state = ProbeState::Pending(reason_fn());
            probe.state = new_state.clone();
            changes.push((ProbeRef::new(svc_name, probe_name), new_state, prev));
        }
    }
    changes
}

/// Activate a target and all its transitive dependency targets.
pub fn activate_target_transitive(
    targets: &mut indexmap::IndexMap<String, crate::model::TargetRuntime>,
    target_name: &str,
) {
    let mut queue = vec![target_name.to_string()];
    while let Some(name) = queue.pop() {
        if let Some(tgt) = targets.get_mut(&name)
            && !tgt.activated
        {
            tgt.activated = true;
            queue.extend(tgt.depends_on_targets.clone());
        }
    }
}

/// Propagate pending state downstream for all probes in a service.
/// Returns collected state changes from propagation.
pub fn propagate_all_pending(
    graph: &crate::graph::DependencyGraph,
    svc_name: &str,
    services: &mut indexmap::IndexMap<String, crate::model::ServiceRuntime>,
) -> Vec<(ProbeRef, ProbeState, ProbeState)> {
    let probe_names: Vec<String> = services[svc_name].probes.keys().cloned().collect();
    let mut changes = Vec::new();
    for probe_name in &probe_names {
        graph.propagate_pending(&format!("{svc_name}.{probe_name}"), services, &mut changes);
    }
    changes
}

/// Determine the new state for a probe that passed, based on dependency satisfaction.
/// Returns (new_state, dep_error_string). Single pass over deps.
fn resolve_ok_state(
    services: &indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    probe_ref: &ProbeRef,
) -> (ProbeState, Option<String>) {
    let deps = &services[&probe_ref.service].probes[&probe_ref.probe].depends_on;
    if deps.is_empty() {
        return (ProbeState::Green, None);
    }

    let mut first_red: Option<ProbeRef> = None;
    let mut first_non_green: Option<ProbeRef> = None;

    for dep in deps {
        let dep_state = services
            .get(&dep.service)
            .and_then(|s| s.probes.get(&dep.probe));
        match dep_state {
            Some(p) if p.state.is_green() => {} // ok
            Some(p) if p.state.is_red() => {
                if first_red.is_none() {
                    first_red = Some(dep.clone());
                }
            }
            None => {
                // Missing dep treated as red
                if first_red.is_none() {
                    first_red = Some(dep.clone());
                }
            }
            _ => {
                // Pending/probing
                if first_non_green.is_none() {
                    first_non_green = Some(dep.clone());
                }
            }
        }
    }

    if let Some(dep) = first_red {
        (
            ProbeState::Red(crate::model::RedReason::DepRed { dep }),
            Some("dep red".into()),
        )
    } else if let Some(dep) = first_non_green {
        (
            ProbeState::Pending(crate::model::PendingReason::DepNotReady { dep }),
            Some("deps not all green".into()),
        )
    } else {
        (ProbeState::Green, None)
    }
}

/// Apply a probe state change within the already-locked `services` map.
///
/// Sets probe fields, emits the state-change event, records the status,
/// and propagates (recovery if went green, pending if went red).
async fn apply_result_inner(
    state: &AppState,
    probe_ref: &ProbeRef,
    new_state: ProbeState,
    duration_ms: u64,
    error: Option<String>,
    services: &mut indexmap::IndexMap<String, crate::model::ServiceRuntime>,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) -> std::collections::HashSet<String> {
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
    probe.last_error = error.clone();

    // Invariant: green probe must have all deps green
    if new_state.is_green() {
        debug_assert!(
            services[&probe_ref.service].probes[&probe_ref.probe]
                .depends_on
                .iter()
                .all(|d| services
                    .get(&d.service)
                    .and_then(|s| s.probes.get(&d.probe))
                    .is_some_and(|p| p.state.is_green())),
            "green probe {} has non-green dep",
            probe_ref
        );
    }

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
            error,
            logs: None,
        },
    );

    let mut affected_services = std::collections::HashSet::new();

    // Recovery propagation: when a probe goes green, mark red reverse-deps as pending
    if new_state.is_green() && !prev.is_green() {
        let graph = &state.graph;
        let mut recovery_changes = Vec::new();
        graph.propagate_recovery(&probe_ref.to_string(), services, &mut recovery_changes);
        for (cr, _, _) in &recovery_changes {
            affected_services.insert(cr.service.clone());
        }
        emit_propagated_changes(state, services, &recovery_changes, probe_statuses);
    }

    // Red propagation: when a probe goes red, propagate to dependents
    if new_state.is_red() && !prev.is_red() {
        let graph = &state.graph;
        let mut red_changes = Vec::new();
        graph.propagate_pending(&probe_ref.to_string(), services, &mut red_changes);
        for (cr, _, _) in &red_changes {
            affected_services.insert(cr.service.clone());
        }
        emit_propagated_changes(state, services, &red_changes, probe_statuses);
    }

    affected_services
}

/// Apply a probe outcome: emit attempt events, update probe state, record in probe_statuses.
///
/// A single write lock on `services` covers both the dependency check and the
/// state update, preventing any TOCTOU race.
/// Apply a probe result and return the set of services affected by propagation.
pub async fn apply_probe_result(
    state: &AppState,
    probe_ref: &ProbeRef,
    outcome: &crate::probe::ProbeOutcome,
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) -> std::collections::HashSet<String> {
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

    // Guard: if the service generation has changed (restart, stop, crash) since the
    // probe was dispatched, discard the stale result.
    let current_gen = services
        .get(&probe_ref.service)
        .map(|s| s.generation)
        .unwrap_or(0);
    if outcome.generation != u64::MAX && current_gen != outcome.generation {
        return std::collections::HashSet::new();
    }

    // 2. Compute new state from outcome, then apply uniformly.
    let (new_state, duration_ms, error) = match &outcome.result {
        crate::probe::ProbeResult::Ok { duration_ms } => {
            let (resolved, dep_error) = resolve_ok_state(&services, probe_ref);
            (resolved, *duration_ms, dep_error)
        }
        crate::probe::ProbeResult::Failed { error, duration_ms } => {
            let new_state = ProbeState::Red(crate::model::RedReason::ProbeFailed(
                crate::model::ProbeFailure {
                    error: error.clone(),
                    duration_ms: *duration_ms,
                },
            ));
            (new_state, *duration_ms, Some(error.clone()))
        }
    };
    let propagated = apply_result_inner(
        state,
        probe_ref,
        new_state,
        duration_ms,
        error,
        &mut services,
        probe_statuses,
    )
    .await;

    // 4. Attach last matched log line to the probe status and persist on ProbeRuntime.
    // (Reuse the existing write lock on services — don't acquire a second one.)
    if !outcome.matched_lines.is_empty() {
        if let Some(status) = probe_statuses.get_mut(&probe_ref.to_string()) {
            status.logs = outcome.matched_lines.last().map(|l| vec![l.clone()]);
        }
        if let Some(svc) = services.get_mut(&probe_ref.service)
            && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
        {
            probe.last_log_match = outcome.matched_lines.last().cloned();
        }
    }

    propagated
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
            .filter(|(_, probe)| probe.is_meta())
            .map(|(name, _)| name.clone())
            .collect()
    };
    for probe_name in meta_probes {
        let probe_ref = ProbeRef::new(service_name, &probe_name);
        let satisfied = crate::probe::meta::is_satisfied(&probe_ref, &services);
        let new_state = if satisfied {
            ProbeState::Green
        } else {
            let (state_from_deps, _) = resolve_ok_state(&services, &probe_ref);
            state_from_deps
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
/// After applying all probes, updates meta probes and re-applies any probes
/// whose deps changed (cross-service meta deps), repeating until stable.
pub async fn resolve_probe_batch(
    state: &AppState,
    batch: &[(ProbeRef, crate::probe::ProbeOutcome)],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) -> std::collections::HashSet<String> {
    use std::collections::HashMap;

    let mut result_map: HashMap<String, &crate::probe::ProbeOutcome> = HashMap::new();
    for (probe_ref, outcome) in batch {
        result_map.insert(probe_ref.to_string(), outcome);
    }

    let probe_topo_order = state.graph.probe_topo_order();
    let mut affected = std::collections::HashSet::new();

    // Apply all probe results in topo order
    for probe_key in probe_topo_order {
        if let Some(outcome) = result_map.get(probe_key) {
            let probe_ref = crate::model::ProbeRef::parse(probe_key).unwrap();
            affected.insert(probe_ref.service.clone());
            let propagated = apply_probe_result(state, &probe_ref, outcome, probe_statuses).await;
            affected.extend(propagated);
        }
    }

    // Update meta probes, then re-apply probes whose deps changed.
    // Repeat until stable (meta → probe → meta cascading).
    for _ in 0..5 {
        // Update all meta probes
        for svc in &affected {
            update_meta_probes(state, svc, probe_statuses).await;
        }

        // Re-apply probes that are Pending(DepNotReady) — their deps weren't green
        // when first applied but may be now (e.g. meta probes resolved above).
        // Only DepNotReady needs re-apply. Other pending states (Reprobing, DepRecovered)
        // are from propagation and shouldn't be re-applied with the same batch result.
        let mut progress = false;
        for probe_key in probe_topo_order {
            if let Some(outcome) = result_map.get(probe_key) {
                let probe_ref = crate::model::ProbeRef::parse(probe_key).unwrap();
                let needs_reapply = {
                    let services = state.services.read().await;
                    let probe = &services[&probe_ref.service].probes[&probe_ref.probe];
                    matches!(
                        probe.state,
                        crate::model::ProbeState::Pending(
                            crate::model::PendingReason::DepNotReady { .. }
                        )
                    )
                };
                if needs_reapply {
                    let propagated =
                        apply_probe_result(state, &probe_ref, outcome, probe_statuses).await;
                    affected.extend(propagated);
                    progress = true;
                }
            }
        }
        if !progress {
            break;
        }
    }

    emit_svc_display_states(state).await;
    affected
}

/// A pending probe ready to be fired.
pub struct PendingProbe {
    pub probe_ref: ProbeRef,
    pub service_name: String,
    pub container: String,
    pub config: ProbeConfig,
    pub log_since: i64,
    pub generation: u64,
}

/// Collect pending non-meta probes. Optionally scoped to specific probe refs.
pub async fn collect_pending_probes(
    state: &AppState,
    scope: Option<&[ProbeRef]>,
) -> Vec<PendingProbe> {
    use crate::model::ServiceState;
    let services = state.services.read().await;
    let mut result = Vec::new();

    let mut try_add = |pr: ProbeRef, svc: &crate::model::ServiceRuntime| {
        if let Some(probe) = svc.probes.get(&pr.probe)
            && probe.state.is_pending()
            && !probe.is_meta()
        {
            result.push(PendingProbe {
                probe_ref: pr,
                service_name: svc.name.clone(),
                container: svc.container.clone(),
                config: probe.probe_config.clone(),
                log_since: svc.log_since,
                generation: svc.generation,
            });
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
                if matches!(svc.state, ServiceState::Stopped | ServiceState::Crashed) {
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

/// Probe pending probes in parallel (single attempt), resolve in dep order as results stream.
/// Fires all probes, streams results: failed applied immediately, ok applied when deps green,
/// held pending otherwise. Cascades to probes whose deps become green.
pub async fn probe_and_resolve(
    state: &AppState,
    pending_probes: &[PendingProbe],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    timeout: std::time::Duration,
) {
    use futures::StreamExt;
    use std::collections::{HashMap, HashSet};

    if pending_probes.is_empty() {
        return;
    }

    let docker = state.docker.inner();
    let mut futs = futures::stream::FuturesUnordered::new();
    for pending in pending_probes {
        let docker = docker.clone();
        let svc = pending.service_name.clone();
        let ctr = pending.container.clone();
        let pc = pending.config.clone();
        let cr = pending.probe_ref.clone();
        let ls = pending.log_since;
        let probe_gen = pending.generation;
        futs.push(async move {
            let mut result =
                crate::probe::run_single_attempt(&docker, &svc, &ctr, &pc, timeout, ls).await;
            result.generation = probe_gen;
            (cr, result)
        });
    }

    let mut pending: HashMap<String, crate::probe::ProbeOutcome> = HashMap::new();

    while let Some((probe_ref, outcome)) = futs.next().await {
        let key = probe_ref.to_string();
        if !outcome.result.is_ok() {
            apply_probe_result(state, &probe_ref, &outcome, probe_statuses).await;
        } else {
            let deps_green = {
                let services = state.services.read().await;
                let probe = &services[&probe_ref.service].probes[&probe_ref.probe];
                probe.depends_on.iter().all(|dep| {
                    services
                        .get(&dep.service)
                        .and_then(|s| s.probes.get(&dep.probe))
                        .is_some_and(|p| p.state.is_green())
                })
            };
            if deps_green {
                apply_probe_result(state, &probe_ref, &outcome, probe_statuses).await;
            } else {
                pending.insert(key, outcome);
                continue;
            }
        }

        // Cascade pending probes whose deps just went green
        let mut affected_svcs: HashSet<String> = HashSet::new();
        affected_svcs.insert(probe_ref.service.clone());
        let mut progress = true;
        while progress {
            progress = false;
            let keys: Vec<String> = pending.keys().cloned().collect();
            for pkey in keys {
                let pcr = ProbeRef::parse(&pkey).unwrap();
                let deps_ok = {
                    let services = state.services.read().await;
                    let probe = &services[&pcr.service].probes[&pcr.probe];
                    probe.depends_on.iter().all(|dep| {
                        services
                            .get(&dep.service)
                            .and_then(|s| s.probes.get(&dep.probe))
                            .is_some_and(|p| p.state.is_green())
                    })
                };
                if deps_ok {
                    let outcome = pending.remove(&pkey).unwrap();
                    apply_probe_result(state, &pcr, &outcome, probe_statuses).await;
                    affected_svcs.insert(pcr.service.clone());
                    progress = true;
                }
            }
        }

        for svc in &affected_svcs {
            update_meta_probes(state, svc, probe_statuses).await;
        }
    }

    // Apply remaining pending (deps never resolved)
    if !pending.is_empty() {
        let remaining: Vec<(ProbeRef, crate::probe::ProbeOutcome)> = pending
            .into_iter()
            .map(|(k, v)| (ProbeRef::parse(&k).unwrap(), v))
            .collect();
        resolve_probe_batch(state, &remaining, probe_statuses).await;
    }
}

/// Compute human-readable reason for a non-green service.
/// Shared by emit_svc_display_states, API routes, and state computation.
pub fn compute_svc_reason(
    display: crate::model::SvcDisplayState,
    svc: &crate::model::ServiceRuntime,
) -> Option<String> {
    use crate::model::SvcDisplayState;
    match (display, svc.state) {
        (SvcDisplayState::Red, crate::model::ServiceState::Stopped) => Some("stopped".into()),
        (SvcDisplayState::Red, crate::model::ServiceState::Crashed) => Some("crashed".into()),
        (SvcDisplayState::Red, _) => {
            let first_red = svc.probes.values().find(|p| p.state.is_red());
            match first_red.map(|p| &p.state) {
                Some(crate::model::ProbeState::Red(crate::model::RedReason::ProbeFailed(_))) => {
                    Some("probe failed".into())
                }
                Some(crate::model::ProbeState::Red(crate::model::RedReason::DepRed { dep })) => {
                    Some(format!("waiting for {}", dep.service))
                }
                _ => {
                    if svc.probes.values().any(|p| p.state.is_pending()) {
                        Some("probes pending".into())
                    } else {
                        None
                    }
                }
            }
        }
        _ => None,
    }
}

/// Compute probe-level detail for a red running service.
pub fn compute_svc_detail(
    display: crate::model::SvcDisplayState,
    svc: &crate::model::ServiceRuntime,
) -> Option<String> {
    use crate::model::SvcDisplayState;
    match (display, svc.state) {
        (SvcDisplayState::Red, crate::model::ServiceState::Running) => {
            let first_red = svc.probes.values().find(|p| p.state.is_red());
            first_red.and_then(|p| {
                p.state
                    .state_detail()
                    .map(|d| format!("{} → {d}", p.probe_ref))
            })
        }
        _ => None,
    }
}

/// Emit service display state events for all services (only emits on change).
/// Scans all services — cheap because it skips unchanged ones via last_emitted_display.
pub async fn emit_svc_display_states(state: &AppState) {
    use crate::model::{SvcDisplayState, active_services};
    let active = {
        let services = state.services.read().await;
        let targets = state.targets.read().await;
        active_services(&services, &targets)
    };
    let mut services = state.services.write().await;
    let svc_names: Vec<String> = services.keys().cloned().collect();
    for svc_name in &svc_names {
        if let Some(svc) = services.get_mut(svc_name) {
            let is_active = active.contains(svc_name.as_str());
            let display = SvcDisplayState::from_service_active(svc, is_active);
            let display_changed = svc.last_emitted_display != Some(display);
            let runtime_changed = svc.last_emitted_runtime != Some(svc.state);
            if display_changed || runtime_changed {
                svc.last_emitted_display = Some(display);
                svc.last_emitted_runtime = Some(svc.state);
                let reason = compute_svc_reason(display, svc);
                let svc_detail = compute_svc_detail(display, svc);
                state.events.emit(Event::service_state(
                    svc_name,
                    svc.state,
                    display.as_str(),
                    reason,
                    svc_detail,
                ));
            }
        }
    }
}

/// Compute and emit target states for targets affected by the given services.
/// Returns only affected targets in the REST response. Emits WS events only for state changes.
/// Walk a red probe's DepRed chain to find the root cause and produce a
/// human-readable reason for the target display.
pub fn target_reason(
    probe: &ProbeRef,
    services: &indexmap::IndexMap<String, crate::model::ServiceRuntime>,
) -> Option<String> {
    let mut current = probe.clone();
    // Follow DepRed chain (bounded to avoid infinite loops)
    for _ in 0..50 {
        let svc = services.get(&current.service)?;
        let p = svc.probes.get(&current.probe)?;
        match &p.state {
            ProbeState::Red(crate::model::RedReason::DepRed { dep }) => {
                current = dep.clone();
            }
            ProbeState::Red(crate::model::RedReason::ProbeFailed(_)) => {
                return Some(format!("probe {} failed", current));
            }
            ProbeState::Red(crate::model::RedReason::Stopped) => {
                return Some(format!("service {} stopped", current.service));
            }
            ProbeState::Red(crate::model::RedReason::ContainerDied) => {
                return Some(format!("service {} crashed", current.service));
            }
            ProbeState::Pending(_) => {
                return Some("probes pending".into());
            }
            _ => return None,
        }
    }
    // Chain too long — use the last dep we found
    Some(format!("waiting for {}", current.service))
}

/// Compute deduplicated human-readable reasons for a red target state.
/// Shared by emit_target_states, api/state.rs compute_display, and api/routes.rs get_target.
pub fn compute_target_reasons(
    target_state: &crate::model::TargetState,
    services: &indexmap::IndexMap<String, crate::model::ServiceRuntime>,
) -> Vec<String> {
    if let crate::model::TargetState::Red {
        probes,
        dep_targets,
    } = target_state
    {
        let mut reasons: Vec<String> = Vec::new();
        for dt in dep_targets {
            let r = format!("target {dt} red");
            if !reasons.contains(&r) {
                reasons.push(r);
            }
        }
        for probe in probes {
            if let Some(r) = target_reason(probe, services)
                && !reasons.contains(&r)
            {
                reasons.push(r);
            }
        }
        reasons
    } else {
        vec![]
    }
}

pub async fn emit_target_states(
    state: &AppState,
    affected_services: &[&str],
) -> indexmap::IndexMap<String, TargetStatus> {
    let services = state.services.read().await;
    let mut targets = state.targets.write().await;

    // First pass: compute states (read-only iteration over targets).
    // state() needs &targets for depends_on lookups, which conflicts with
    // iter_mut(), so compute all states first then apply changes.
    let mut computed: Vec<(String, crate::model::TargetState)> = Vec::new();
    for (name, tgt) in targets.iter() {
        let affected = affected_services.is_empty()
            || tgt
                .transitive_probes
                .iter()
                .any(|c| affected_services.contains(&c.service.as_str()));
        if !affected {
            continue;
        }
        let current = tgt.state(&services, &targets);
        computed.push((name.clone(), current));
    }

    // Second pass: apply state changes + emit events
    let mut statuses = indexmap::IndexMap::new();
    for (name, current) in computed {
        let tgt = targets.get_mut(&name).unwrap();
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
            let reasons = compute_target_reasons(&current, &services);
            let (reason_override, extra_reasons) = if reasons.is_empty() {
                (None, vec![])
            } else if reasons.len() == 1 {
                (Some(reasons[0].clone()), vec![])
            } else {
                let summary = format!("{} (+{} more)", reasons[0], reasons.len() - 1);
                (Some(summary), reasons)
            };
            tgt.last_emitted_state = Some(current.clone());
            state.events.emit(Event::target_state(
                &name,
                current,
                None,
                reason_override,
                extra_reasons,
            ));
        }
    }
    statuses
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GantryConfig;
    use crate::graph::DependencyGraph;
    use crate::model::{ProbeState, RuntimeState, ServiceState};
    use crate::probe::{ProbeOutcome, ProbeResult};

    /// Build a test AppState from YAML config with a dummy Docker client.
    fn test_app_state(config: &GantryConfig) -> std::sync::Arc<crate::api::AppState> {
        let graph = DependencyGraph::build(config).unwrap();
        let mut runtime = RuntimeState::from_config(config);
        for tgt in runtime.targets.values_mut() {
            tgt.activated = true;
        }
        for (tgt_name, tgt) in runtime.targets.iter_mut() {
            tgt.transitive_probes = graph.flatten_target(tgt_name, config);
        }
        crate::api::AppState::new(
            config.clone(),
            graph,
            runtime,
            crate::docker::DockerClient::dummy(),
        )
    }

    /// Test that resolve_probe_batch handles cross-service meta deps correctly.
    /// Setup: flaky-api has dep + http + ready(meta).
    ///        doomed has dep (depends on flaky-api.ready) + http.
    /// All probes pass. After resolve_probe_batch, doomed.dep must be green
    /// (not pending), even though flaky-api.ready is a meta probe resolved mid-batch.
    #[tokio::test]
    async fn resolve_batch_cross_service_meta_deps() {
        let yaml = r#"
services:
  flaky-api:
    container: test-flaky-api
    probes:
      dep:
        probe: { type: log, success: "connected", timeout: 5s }
      http:
        probe: { type: tcp, port: 8080, timeout: 5s }
        depends_on: [flaky-api.dep]
  doomed:
    container: test-doomed
    start_after: [flaky-api.ready]
    probes:
      dep:
        probe: { type: log, success: "connected", timeout: 5s }
        depends_on: [flaky-api.ready]
      http:
        probe: { type: tcp, port: 8080, timeout: 5s }
        depends_on: [doomed.dep]
targets:
  all:
    probes: [doomed.ready]
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.auto_generate_ready_probes();
        config.topo_sort_probes();
        let state = test_app_state(&config);

        // Set all services to Running with pending probes (simulates reprobe_all)
        {
            let mut services = state.services.write().await;
            for svc in services.values_mut() {
                svc.state = ServiceState::Running;
                for probe in svc.probes.values_mut() {
                    probe.state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
                }
            }
        }

        // All probes pass — build Ok results for all non-meta probes
        let batch: Vec<(crate::model::ProbeRef, ProbeOutcome)> = vec![
            (
                crate::model::ProbeRef::new("flaky-api", "dep"),
                ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 1 }),
            ),
            (
                crate::model::ProbeRef::new("flaky-api", "http"),
                ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 1 }),
            ),
            (
                crate::model::ProbeRef::new("doomed", "dep"),
                ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 1 }),
            ),
            (
                crate::model::ProbeRef::new("doomed", "http"),
                ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 1 }),
            ),
        ];

        let mut probe_statuses = indexmap::IndexMap::new();
        resolve_probe_batch(&state, &batch, &mut probe_statuses).await;

        // Verify all probes are green (including cross-service meta dep chain)
        let services = state.services.read().await;
        for (svc_name, svc) in services.iter() {
            for (probe_name, probe) in &svc.probes {
                assert!(
                    probe.state.is_green(),
                    "{svc_name}.{probe_name} should be green but is {:?}",
                    probe.state
                );
            }
        }
    }

    // ── Shared config for apply/meta/trace tests ──

    fn two_svc_config() -> GantryConfig {
        let yaml = r#"
services:
  db:
    container: test-db
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 5s }
      ready:
        probe: { type: log, success: "ready", timeout: 5s }
        depends_on: [db.port]
  app:
    container: test-app
    start_after: [db.ready]
    probes:
      http:
        probe: { type: tcp, port: 8080, timeout: 5s }
        depends_on: [db.ready]
targets:
  all:
    probes: [app.ready]
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.auto_generate_ready_probes();
        config.topo_sort_probes();
        config
    }

    async fn setup_running(state: &crate::api::AppState) {
        let mut services = state.services.write().await;
        for svc in services.values_mut() {
            svc.state = ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }
    }

    // ── apply_probe_result tests ──

    #[tokio::test]
    async fn apply_probe_ok_all_deps_green() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // Mark app.http pending, but db.ready is green
        {
            let mut services = state.services.write().await;
            services
                .get_mut("app")
                .unwrap()
                .probes
                .get_mut("http")
                .unwrap()
                .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
        }

        let outcome = ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 5 });
        let mut statuses = indexmap::IndexMap::new();
        let pr = crate::model::ProbeRef::new("app", "http");
        apply_probe_result(&state, &pr, &outcome, &mut statuses).await;

        let services = state.services.read().await;
        assert!(services["app"].probes["http"].state.is_green());
    }

    #[tokio::test]
    async fn apply_probe_ok_dep_red() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // db.ready is red, app.http probe passes
        {
            let mut services = state.services.write().await;
            services
                .get_mut("db")
                .unwrap()
                .probes
                .get_mut("ready")
                .unwrap()
                .state = ProbeState::Red(crate::model::RedReason::ProbeFailed(
                crate::model::ProbeFailure {
                    error: "timeout".into(),
                    duration_ms: 5000,
                },
            ));
            services
                .get_mut("app")
                .unwrap()
                .probes
                .get_mut("http")
                .unwrap()
                .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
        }

        let outcome = ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 5 });
        let mut statuses = indexmap::IndexMap::new();
        let pr = crate::model::ProbeRef::new("app", "http");
        apply_probe_result(&state, &pr, &outcome, &mut statuses).await;

        // Probe passed but dep is red → stays Red(DepRed)
        let services = state.services.read().await;
        assert!(
            services["app"].probes["http"].state.is_red(),
            "probe should be red when dep is red, got {:?}",
            services["app"].probes["http"].state
        );
    }

    #[tokio::test]
    async fn apply_probe_ok_dep_pending() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // db.ready is pending, app.http probe passes
        {
            let mut services = state.services.write().await;
            services
                .get_mut("db")
                .unwrap()
                .probes
                .get_mut("ready")
                .unwrap()
                .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
            services
                .get_mut("app")
                .unwrap()
                .probes
                .get_mut("http")
                .unwrap()
                .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
        }

        let outcome = ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 5 });
        let mut statuses = indexmap::IndexMap::new();
        let pr = crate::model::ProbeRef::new("app", "http");
        apply_probe_result(&state, &pr, &outcome, &mut statuses).await;

        // Probe passed but dep is pending → stays Pending(DepNotReady)
        let services = state.services.read().await;
        assert!(
            services["app"].probes["http"].state.is_pending(),
            "probe should be pending when dep is pending, got {:?}",
            services["app"].probes["http"].state
        );
    }

    #[tokio::test]
    async fn apply_probe_failed() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        let outcome = ProbeOutcome::immediate(ProbeResult::Failed {
            error: "connection refused".into(),
            duration_ms: 100,
        });
        let mut statuses = indexmap::IndexMap::new();
        let pr = crate::model::ProbeRef::new("app", "http");
        apply_probe_result(&state, &pr, &outcome, &mut statuses).await;

        let services = state.services.read().await;
        assert!(services["app"].probes["http"].state.is_red());
    }

    // ── update_meta_probes tests ──

    #[tokio::test]
    async fn meta_probe_green_when_deps_green() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // db.port and db.ready are green → db.ready (meta=auto-generated) should be green
        // app.http is green → app.ready should be green
        let mut statuses = indexmap::IndexMap::new();
        update_meta_probes(&state, "app", &mut statuses).await;

        let services = state.services.read().await;
        assert!(services["app"].probes["ready"].state.is_green());
    }

    #[tokio::test]
    async fn meta_probe_red_when_dep_red() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // Make app.http red
        {
            let mut services = state.services.write().await;
            services
                .get_mut("app")
                .unwrap()
                .probes
                .get_mut("http")
                .unwrap()
                .state = ProbeState::Red(crate::model::RedReason::ProbeFailed(
                crate::model::ProbeFailure {
                    error: "fail".into(),
                    duration_ms: 0,
                },
            ));
        }

        let mut statuses = indexmap::IndexMap::new();
        update_meta_probes(&state, "app", &mut statuses).await;

        let services = state.services.read().await;
        assert!(
            !services["app"].probes["ready"].state.is_green(),
            "meta probe should not be green when dep is red"
        );
    }

    #[tokio::test]
    async fn meta_probe_pending_when_dep_pending() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // Make app.http pending (not red) — meta probe should be Pending, not Red
        {
            let mut services = state.services.write().await;
            services
                .get_mut("app")
                .unwrap()
                .probes
                .get_mut("http")
                .unwrap()
                .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
        }

        let mut statuses = indexmap::IndexMap::new();
        update_meta_probes(&state, "app", &mut statuses).await;

        let services = state.services.read().await;
        assert!(
            services["app"].probes["ready"].state.is_pending(),
            "meta probe should be pending when dep is pending, got {:?}",
            services["app"].probes["ready"].state
        );
    }

    // ── mark_pending_and_propagate test ──

    #[tokio::test]
    async fn mark_pending_propagates_downstream() {
        let config = two_svc_config();
        let state = test_app_state(&config);
        setup_running(&state).await;

        // Mark db.ready pending → app.http (depends on db.ready) should go pending too
        let mut statuses = indexmap::IndexMap::new();
        {
            let mut services = state.services.write().await;
            let graph = &state.graph;
            let mut changes = Vec::new();
            let pr = crate::model::ProbeRef::new("db", "ready");
            let probe = services
                .get_mut("db")
                .unwrap()
                .probes
                .get_mut("ready")
                .unwrap();
            probe.state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
            changes.push((pr.clone(), probe.state.clone(), ProbeState::Green));
            graph.propagate_pending(&pr.to_string(), &mut services, &mut changes);
            emit_propagated_changes(&state, &services, &changes, &mut statuses);
        }

        let services = state.services.read().await;
        assert!(
            services["app"].probes["http"].state.is_pending(),
            "app.http should be pending after db.ready went pending, got {:?}",
            services["app"].probes["http"].state
        );
    }
}
