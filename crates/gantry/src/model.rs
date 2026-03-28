use indexmap::IndexMap;
use serde::Serialize;
use std::fmt;

use crate::config::{GantryConfig, ProbeConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Stopped,
    Running,
    Crashed,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Crashed => "crashed",
        }
    }
}

impl fmt::Display for ServiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum ProbeState {
    Green,
    Red(RedReason),
    Pending(PendingReason),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum RedReason {
    ProbeFailed(ProbeFailure),
    DepRed { dep: ProbeRef },
    Stopped,
    ContainerDied,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum PendingReason {
    DepRecovered { dep: ProbeRef },
    DepNotReady { dep: ProbeRef },
    ContainerStarted,
    Reprobing,
    Unchecked,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeFailure {
    pub error: String,
    pub duration_ms: u64,
}

impl RedReason {
    /// Full reason with entity/error detail (for terminal log / API).
    pub fn display(&self) -> String {
        match self {
            Self::ProbeFailed(f) => format!("probe failed: {} ({}ms)", f.error, f.duration_ms),
            Self::DepRed { dep } => format!("dep red: {dep}"),
            Self::Stopped => "stopped".into(),
            Self::ContainerDied => "container died".into(),
        }
    }

    /// Short reason without entity/error detail (for UI summary).
    pub fn short_display(&self) -> &'static str {
        match self {
            Self::ProbeFailed(_) => "probe failed",
            Self::DepRed { .. } => "dep red",
            Self::Stopped => "stopped",
            Self::ContainerDied => "container died",
        }
    }

    /// Detail text for UI expand (entity name or error info).
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::ProbeFailed(f) => Some(format!("{} ({}ms)", f.error, f.duration_ms)),
            Self::DepRed { dep } => Some(dep.to_string()),
            _ => None,
        }
    }
}

impl PendingReason {
    /// Full reason with entity detail (for terminal log / API).
    pub fn display(&self) -> String {
        match self {
            Self::DepRecovered { dep } => format!("dep {dep} recovered"),
            Self::DepNotReady { dep } => format!("dep {dep} not ready"),
            Self::ContainerStarted => "container started".into(),
            Self::Reprobing => "reprobing".into(),
            Self::Unchecked => "unchecked".into(),
        }
    }

    /// Short reason without entity detail (for UI summary).
    pub fn short_display(&self) -> &'static str {
        match self {
            Self::DepRecovered { .. } => "dep recovered",
            Self::DepNotReady { .. } => "dep not ready",
            Self::ContainerStarted => "container started",
            Self::Reprobing => "reprobing",
            Self::Unchecked => "unchecked",
        }
    }

    /// Detail text for UI expand (dep entity name).
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::DepRecovered { dep } => Some(dep.to_string()),
            Self::DepNotReady { dep } => Some(dep.to_string()),
            _ => None,
        }
    }
}

impl ProbeState {
    /// Full reason with entity/error detail (for terminal log / API).
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Green => None,
            Self::Red(r) => Some(r.display()),
            Self::Pending(r) => Some(r.display()),
        }
    }

    /// Short reason without entity/error detail (for UI summary).
    pub fn short_reason(&self) -> Option<&'static str> {
        match self {
            Self::Green => None,
            Self::Red(r) => Some(r.short_display()),
            Self::Pending(r) => Some(r.short_display()),
        }
    }

    /// Detail text for UI expand.
    pub fn state_detail(&self) -> Option<String> {
        match self {
            Self::Green => None,
            Self::Red(r) => r.detail(),
            Self::Pending(r) => r.detail(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red(_) => "red",
            Self::Pending(_) => "pending",
        }
    }

    pub fn color(&self) -> ProbeColor {
        match self {
            Self::Green => ProbeColor::Green,
            Self::Red(_) => ProbeColor::Red,
            Self::Pending(_) => ProbeColor::Pending,
        }
    }

    pub fn is_green(&self) -> bool {
        matches!(self, Self::Green)
    }

    pub fn is_red(&self) -> bool {
        matches!(self, Self::Red(_))
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    pub fn is_probe_failed(&self) -> bool {
        matches!(self, Self::Red(RedReason::ProbeFailed(_)))
    }
}

impl fmt::Display for ProbeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeColor {
    Green,
    Red,
    Pending,
}

impl ProbeColor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Pending => "pending",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum TargetState {
    Green,
    Red {
        /// All non-green probes (first is primary for summary, rest in detail).
        probes: Vec<ProbeRef>,
        /// Depends_on targets that are not green.
        dep_targets: Vec<String>,
    },
    Inactive,
}

impl TargetState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red { .. } => "red",
            Self::Inactive => "inactive",
        }
    }

    /// First red probe (for API/data).
    pub fn first_red_probe(&self) -> Option<&ProbeRef> {
        match self {
            Self::Red { probes, .. } => probes.first(),
            _ => None,
        }
    }

    pub fn is_green(&self) -> bool {
        matches!(self, Self::Green)
    }
}

impl fmt::Display for TargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Display state for a probe, accounting for whether its service is running.
/// Green/Red are outcome states. Probing = actively being checked (pulsing in UI).
/// Pending = needs check but not active yet. Stopped = service not running.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeDisplayState {
    Green,
    Red,
    Probing,
    Pending,
    Stopped,
}

impl ProbeDisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Probing => "probing",
            Self::Pending => "pending",
            Self::Stopped => "stopped",
        }
    }

    pub fn from_probe(probe: &ProbeRuntime, svc_state: ServiceState) -> Self {
        match svc_state {
            ServiceState::Stopped => Self::Stopped,
            ServiceState::Crashed => Self::Red,
            _ => match &probe.state {
                ProbeState::Green => Self::Green,
                ProbeState::Red(_) => Self::Red,
                ProbeState::Pending(PendingReason::Reprobing) => Self::Probing,
                ProbeState::Pending(_) => Self::Pending,
            },
        }
    }
}

/// Display state for a service — outcome-only view.
/// Green = all probes green. Red = not green (reason explains why).
/// Stopped = not running, not needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SvcDisplayState {
    Green,
    Red,
    Stopped,
}

impl SvcDisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Stopped => "stopped",
        }
    }

    pub fn from_service(svc: &ServiceRuntime) -> Self {
        Self::from_service_active(svc, true)
    }

    /// Display state accounting for whether the service is "active" —
    /// reachable from an activated target or a running service's deps.
    /// Stopped + active = Red (blocker); Stopped + inactive = Stopped (gray).
    pub fn from_service_active(svc: &ServiceRuntime, is_active: bool) -> Self {
        match svc.state {
            ServiceState::Stopped if is_active => Self::Red,
            ServiceState::Stopped => Self::Stopped,
            ServiceState::Crashed => Self::Red,
            _ => {
                if svc.probes.values().all(|p| p.state.is_green()) {
                    Self::Green
                } else {
                    Self::Red
                }
            }
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize)]
pub struct ProbeRef {
    pub service: String,
    pub probe: String,
}

impl ProbeRef {
    pub fn new(service: &str, probe: &str) -> Self {
        Self {
            service: service.to_string(),
            probe: probe.to_string(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (svc, probe) = s.split_once('.')?;
        Some(Self::new(svc, probe))
    }
}

impl fmt::Display for ProbeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.service, self.probe)
    }
}

#[derive(Debug, Clone)]
pub struct ServiceRuntime {
    pub name: String,
    pub container: String,
    pub state: ServiceState,
    pub probes: IndexMap<String, ProbeRuntime>,
    pub start_after: Vec<ProbeRef>,
    pub restart_on_fail: bool,
    pub generation: u64,
    /// Unix timestamp (seconds) for log probe `since` parameter.
    /// Updated after each probe run. Ensures log probes only see recent output.
    pub log_since: i64,
    pub last_emitted_display: Option<SvcDisplayState>,
    pub last_emitted_runtime: Option<ServiceState>,
}

#[derive(Debug, Clone)]
pub struct ProbeRuntime {
    pub probe_ref: ProbeRef,
    pub state: ProbeState,
    pub prev_color: Option<ProbeColor>,
    pub probe_config: ProbeConfig,
    pub depends_on: Vec<ProbeRef>,
    pub last_probe_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Last matched log line from a log probe (persisted for API queries).
    pub last_log_match: Option<String>,
}

impl ProbeRuntime {
    pub fn is_meta(&self) -> bool {
        self.probe_config.is_meta()
    }
}

#[derive(Debug, Clone)]
pub struct TargetRuntime {
    pub name: String,
    pub direct_probes: Vec<ProbeRef>,
    pub transitive_probes: Vec<ProbeRef>,
    pub depends_on_targets: Vec<String>,
    pub last_emitted_state: Option<TargetState>,
    /// True once this target has been converged or probed at least once.
    pub activated: bool,
}

impl TargetRuntime {
    /// Target state derived from direct probes + depends_on targets.
    /// Probe dependency graph handles transitivity — if a deep dep is red,
    /// the direct probe will be Red(DepRed) via propagation.
    pub fn state(
        &self,
        services: &IndexMap<String, ServiceRuntime>,
        targets: &IndexMap<String, TargetRuntime>,
    ) -> TargetState {
        let mut visited = std::collections::HashSet::new();
        self.state_inner(services, targets, &mut visited)
    }

    fn state_inner(
        &self,
        services: &IndexMap<String, ServiceRuntime>,
        targets: &IndexMap<String, TargetRuntime>,
        visited: &mut std::collections::HashSet<String>,
    ) -> TargetState {
        if !self.activated {
            return TargetState::Inactive;
        }
        let mut red_probes = Vec::new();
        let mut red_targets = Vec::new();

        // Check depends_on targets (with cycle guard)
        for dep_name in &self.depends_on_targets {
            if !visited.insert(dep_name.clone()) {
                continue; // Already visiting — cycle, skip
            }
            if let Some(dep_tgt) = targets.get(dep_name) {
                let dep_state = dep_tgt.state_inner(services, targets, visited);
                if !dep_state.is_green() && !matches!(dep_state, TargetState::Inactive) {
                    red_targets.push(dep_name.clone());
                }
            }
        }

        // Check direct probes
        for probe_ref in &self.direct_probes {
            let is_green = services
                .get(&probe_ref.service)
                .and_then(|s| s.probes.get(&probe_ref.probe))
                .is_some_and(|p| p.state.is_green());
            if !is_green {
                red_probes.push(probe_ref.clone());
            }
        }

        if red_probes.is_empty() && red_targets.is_empty() {
            TargetState::Green
        } else {
            TargetState::Red {
                probes: red_probes,
                dep_targets: red_targets,
            }
        }
    }
}

/// Compute the set of services that are "active" — reachable from any
/// activated target or from any running service's dependencies.
pub fn active_services(
    services: &IndexMap<String, ServiceRuntime>,
    targets: &IndexMap<String, TargetRuntime>,
) -> std::collections::HashSet<String> {
    let mut active = std::collections::HashSet::new();
    // All running/crashed services are active
    for (name, svc) in services {
        if matches!(svc.state, ServiceState::Running | ServiceState::Crashed) {
            active.insert(name.clone());
        }
    }
    // Services reachable from activated targets
    for tgt in targets.values() {
        if tgt.activated {
            for pr in &tgt.transitive_probes {
                active.insert(pr.service.clone());
            }
        }
    }
    // Services reachable from running services' start_after deps
    for svc in services.values() {
        if svc.state == ServiceState::Running {
            for dep in &svc.start_after {
                active.insert(dep.service.clone());
            }
        }
    }
    active
}

pub struct RuntimeState {
    pub services: IndexMap<String, ServiceRuntime>,
    pub targets: IndexMap<String, TargetRuntime>,
}

impl RuntimeState {
    pub fn from_config(config: &GantryConfig) -> Self {
        let mut services = IndexMap::new();
        for (svc_name, svc_config) in &config.services {
            let mut probes = IndexMap::new();
            for (probe_name, probe_config) in &svc_config.probes {
                let depends_on: Vec<ProbeRef> = probe_config
                    .depends_on
                    .iter()
                    .filter_map(|s| ProbeRef::parse(s))
                    .collect();
                probes.insert(
                    probe_name.clone(),
                    ProbeRuntime {
                        probe_ref: ProbeRef::new(svc_name, probe_name),
                        state: ProbeState::Red(RedReason::Stopped),
                        prev_color: None,
                        probe_config: probe_config.probe.clone(),
                        depends_on,
                        last_probe_ms: None,
                        last_error: None,
                        last_log_match: None,
                    },
                );
            }
            let start_after: Vec<ProbeRef> = svc_config
                .start_after
                .iter()
                .filter_map(|s| ProbeRef::parse(s))
                .collect();
            services.insert(
                svc_name.clone(),
                ServiceRuntime {
                    name: svc_name.clone(),
                    container: svc_config.container.clone(),
                    state: ServiceState::Stopped,
                    probes,
                    start_after,
                    restart_on_fail: svc_config.restart_on_fail(&config.defaults),
                    generation: 0,
                    log_since: 0,
                    last_emitted_display: None,
                    last_emitted_runtime: None,
                },
            );
        }

        let mut targets = IndexMap::new();
        for (tgt_name, tgt_config) in &config.targets {
            let direct_probes: Vec<ProbeRef> = tgt_config
                .probes
                .iter()
                .filter_map(|s| ProbeRef::parse(s))
                .collect();
            targets.insert(
                tgt_name.clone(),
                TargetRuntime {
                    name: tgt_name.clone(),
                    direct_probes: direct_probes.clone(),
                    transitive_probes: direct_probes,
                    depends_on_targets: tgt_config.depends_on.clone(),
                    last_emitted_state: None,
                    activated: false,
                },
            );
        }

        Self { services, targets }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_ref_parse() {
        let pr = ProbeRef::parse("db.port").unwrap();
        assert_eq!(pr.service, "db");
        assert_eq!(pr.probe, "port");
        assert_eq!(pr.to_string(), "db.port");
    }

    #[test]
    fn probe_ref_parse_invalid() {
        assert!(ProbeRef::parse("noperiod").is_none());
    }

    // ── ProbeDisplayState tests ──

    fn make_probe(state: ProbeState) -> ProbeRuntime {
        ProbeRuntime {
            probe_ref: ProbeRef::new("test", "probe"),
            state,
            prev_color: None,
            probe_config: crate::config::ProbeConfig::Meta,
            depends_on: vec![],
            last_probe_ms: None,
            last_error: None,
            last_log_match: None,
        }
    }

    #[test]
    fn probe_display_stopped_service() {
        let probe = make_probe(ProbeState::Green);
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Stopped),
            ProbeDisplayState::Stopped
        ));
    }

    #[test]
    fn probe_display_running_green() {
        let probe = make_probe(ProbeState::Green);
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Green
        ));
    }

    #[test]
    fn probe_display_running_red() {
        let probe = make_probe(ProbeState::Red(RedReason::Stopped));
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Red
        ));
    }

    #[test]
    fn probe_display_running_probing() {
        let probe = make_probe(ProbeState::Pending(PendingReason::Reprobing));
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Probing
        ));
    }

    #[test]
    fn probe_display_running_pending() {
        let probe = make_probe(ProbeState::Pending(PendingReason::Unchecked));
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Pending
        ));
    }

    #[test]
    fn probe_display_crashed_forces_red() {
        // Even if probe is internally Green, crashed service forces Red display
        let probe = make_probe(ProbeState::Green);
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Crashed),
            ProbeDisplayState::Red
        ));
    }

    #[test]
    fn probe_display_crashed_with_pending_probe() {
        let probe = make_probe(ProbeState::Pending(PendingReason::Reprobing));
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Crashed),
            ProbeDisplayState::Red
        ));
    }

    // ── SvcDisplayState tests ──

    fn make_svc(state: ServiceState, probe_states: &[ProbeState]) -> ServiceRuntime {
        let mut probes = IndexMap::new();
        for (i, ps) in probe_states.iter().enumerate() {
            probes.insert(
                format!("probe{i}"),
                ProbeRuntime {
                    probe_ref: ProbeRef::new("test", &format!("probe{i}")),
                    state: ps.clone(),
                    prev_color: None,
                    probe_config: crate::config::ProbeConfig::Meta,
                    depends_on: vec![],
                    last_probe_ms: None,
                    last_error: None,
                    last_log_match: None,
                },
            );
        }
        ServiceRuntime {
            name: "test".into(),
            container: "test-1".into(),
            state,
            probes,
            start_after: vec![],
            restart_on_fail: true,
            generation: 0,
            log_since: 0,
            last_emitted_display: None,
            last_emitted_runtime: None,
        }
    }

    #[test]
    fn svc_display_stopped_inactive() {
        // Stopped service not reachable from active targets → gray/stopped
        let svc = make_svc(
            ServiceState::Stopped,
            &[ProbeState::Red(RedReason::Stopped)],
        );
        assert!(matches!(
            SvcDisplayState::from_service_active(&svc, false),
            SvcDisplayState::Stopped
        ));
    }

    #[test]
    fn svc_display_stopped_active() {
        // Stopped service reachable from active targets → red (blocker)
        let svc = make_svc(
            ServiceState::Stopped,
            &[ProbeState::Red(RedReason::Stopped)],
        );
        assert!(matches!(
            SvcDisplayState::from_service_active(&svc, true),
            SvcDisplayState::Red
        ));
    }

    #[test]
    fn svc_display_crashed() {
        let svc = make_svc(ServiceState::Crashed, &[ProbeState::Green]);
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Red
        ));
    }

    #[test]
    fn svc_display_all_green() {
        let svc = make_svc(
            ServiceState::Running,
            &[ProbeState::Green, ProbeState::Green],
        );
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Green
        ));
    }

    #[test]
    fn svc_display_any_red() {
        let svc = make_svc(
            ServiceState::Running,
            &[ProbeState::Green, ProbeState::Red(RedReason::Stopped)],
        );
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Red
        ));
    }

    #[test]
    fn svc_display_pending_and_red_shows_red() {
        // With pending+red probes, service shows red (not green)
        let svc = make_svc(
            ServiceState::Running,
            &[
                ProbeState::Pending(PendingReason::Reprobing),
                ProbeState::Red(RedReason::Stopped),
            ],
        );
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Red
        ));
    }

    #[test]
    fn svc_display_pending_only_shows_red() {
        // Pending probes mean service is not green → red
        let svc = make_svc(
            ServiceState::Running,
            &[
                ProbeState::Green,
                ProbeState::Pending(PendingReason::Reprobing),
            ],
        );
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Red
        ));
    }

    // ── TargetRuntime::state tests ──

    #[test]
    fn target_state_all_green() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
targets:
  t:
    probes: [db.port]
"#;
        let config: crate::config::GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let mut state = RuntimeState::from_config(&config);
        for tgt in state.targets.values_mut() {
            tgt.activated = true;
        }
        let db = state.services.get_mut("db").unwrap();
        db.state = ServiceState::Running;
        for probe in db.probes.values_mut() {
            probe.state = ProbeState::Green;
        }
        assert!(
            state.targets["t"]
                .state(&state.services, &state.targets)
                .is_green()
        );
    }

    #[test]
    fn target_state_stopped_service_is_red() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
targets:
  t:
    probes: [db.port]
"#;
        let config: crate::config::GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let mut state = RuntimeState::from_config(&config);
        for tgt in state.targets.values_mut() {
            tgt.activated = true;
        }
        // db is Stopped by default, target activated → Red
        assert!(matches!(
            state.targets["t"].state(&state.services, &state.targets),
            TargetState::Red { .. }
        ));
    }

    #[test]
    fn target_state_pending_probe_is_red() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
targets:
  t:
    probes: [db.port]
"#;
        let config: crate::config::GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let mut state = RuntimeState::from_config(&config);
        for tgt in state.targets.values_mut() {
            tgt.activated = true;
        }
        state.services.get_mut("db").unwrap().state = ServiceState::Running;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Pending(PendingReason::Reprobing);
        assert!(matches!(
            state.targets["t"].state(&state.services, &state.targets),
            TargetState::Red { .. }
        ));
    }

    #[test]
    fn target_state_crashed_service_is_red() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
targets:
  t:
    probes: [db.port]
"#;
        let config: crate::config::GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let mut state = RuntimeState::from_config(&config);
        for tgt in state.targets.values_mut() {
            tgt.activated = true;
        }
        state.services.get_mut("db").unwrap().state = ServiceState::Crashed;
        assert!(matches!(
            state.targets["t"].state(&state.services, &state.targets),
            TargetState::Red { .. }
        ));
    }

    #[test]
    fn target_state_inactive_before_activation() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
targets:
  t:
    probes: [db.port]
"#;
        let config: crate::config::GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let state = RuntimeState::from_config(&config);
        // Target not activated → Inactive (not Red)
        assert!(matches!(
            state.targets["t"].state(&state.services, &state.targets),
            TargetState::Inactive
        ));
    }
}
