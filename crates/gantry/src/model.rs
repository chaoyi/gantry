use indexmap::IndexMap;
use serde::Serialize;
use std::fmt;

use crate::config::{GantryConfig, ProbeConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Crashed,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeState {
    Green,
    Red,
    Stale,
}

impl ProbeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Stale => "stale",
        }
    }
}

impl fmt::Display for ProbeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetState {
    Green,
    Red,
    Stale,
    Stopped,
}

impl TargetState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Stale => "stale",
            Self::Stopped => "stopped",
        }
    }
}

impl fmt::Display for TargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Display state for a probe, accounting for whether its service is running.
/// The internal ProbeState model only has Green/Red/Stale. This adds Stopped/Pending
/// for the UI layer.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeDisplayState {
    Green,
    Red,
    Stale,
    Stopped,
}

impl ProbeDisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Stale => "stale",
            Self::Stopped => "stopped",
        }
    }

    pub fn from_probe(probe: &ProbeRuntime, svc_state: ServiceState) -> Self {
        match svc_state {
            ServiceState::Stopped => Self::Stopped,
            _ => match probe.state {
                ProbeState::Green => Self::Green,
                ProbeState::Red => Self::Red,
                ProbeState::Stale => Self::Stale,
            },
        }
    }
}

/// Display state for a service, derived from its probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SvcDisplayState {
    Green,
    Red,
    Stale,
    Stopped,
}

impl SvcDisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Stale => "stale",
            Self::Stopped => "stopped",
        }
    }

    pub fn from_service(svc: &ServiceRuntime) -> Self {
        match svc.state {
            ServiceState::Stopped => Self::Stopped,
            ServiceState::Crashed => Self::Red,
            _ => {
                let mut has_red = false;
                let mut has_stale = false;
                for probe in svc.probes.values() {
                    match probe.state {
                        ProbeState::Red => has_red = true,
                        ProbeState::Stale => has_stale = true,
                        ProbeState::Green => {}
                    }
                }
                if has_stale {
                    return Self::Stale;
                }
                if has_red {
                    return Self::Red;
                }
                Self::Green
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
}

#[derive(Debug, Clone)]
pub struct ProbeRuntime {
    pub probe_ref: ProbeRef,
    pub state: ProbeState,
    pub prev_state: Option<ProbeState>,
    pub probe_config: ProbeConfig,
    pub depends_on: Vec<ProbeRef>,
    pub last_probe_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TargetRuntime {
    pub name: String,
    pub direct_probes: Vec<ProbeRef>,
    pub transitive_probes: Vec<ProbeRef>,
    pub depends_on_targets: Vec<String>,
    pub last_emitted_state: Option<TargetState>,
}

impl TargetRuntime {
    /// Target state derived from transitive probes + service states.
    pub fn state(&self, services: &IndexMap<String, ServiceRuntime>) -> TargetState {
        let mut has_stale = false;
        for probe_ref in &self.transitive_probes {
            let Some(svc) = services.get(&probe_ref.service) else {
                return TargetState::Red;
            };
            if svc.state == ServiceState::Stopped {
                return TargetState::Red; // service down = target unsatisfied
            }
            let Some(probe) = svc.probes.get(&probe_ref.probe) else {
                return TargetState::Red;
            };
            match probe.state {
                ProbeState::Red => return TargetState::Red,
                ProbeState::Stale => has_stale = true,
                ProbeState::Green => {}
            }
        }
        if has_stale {
            TargetState::Stale
        } else {
            TargetState::Green
        }
    }
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
                        state: ProbeState::Red,
                        prev_state: None,
                        probe_config: probe_config.probe.clone(),
                        depends_on,
                        last_probe_ms: None,
                        last_error: None,
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
            prev_state: None,
            probe_config: crate::config::ProbeConfig::Meta,
            depends_on: vec![],
            last_probe_ms: None,
            last_error: None,
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
        let probe = make_probe(ProbeState::Red);
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Red
        ));
    }

    #[test]
    fn probe_display_running_stale() {
        let probe = make_probe(ProbeState::Stale);
        assert!(matches!(
            ProbeDisplayState::from_probe(&probe, ServiceState::Running),
            ProbeDisplayState::Stale
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
                    state: *ps,
                    prev_state: None,
                    probe_config: crate::config::ProbeConfig::Meta,
                    depends_on: vec![],
                    last_probe_ms: None,
                    last_error: None,
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
        }
    }

    #[test]
    fn svc_display_stopped() {
        let svc = make_svc(ServiceState::Stopped, &[ProbeState::Red]);
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Stopped
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
        let svc = make_svc(ServiceState::Running, &[ProbeState::Green, ProbeState::Red]);
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Red
        ));
    }

    #[test]
    fn svc_display_stale_priority_over_red() {
        let svc = make_svc(ServiceState::Running, &[ProbeState::Stale, ProbeState::Red]);
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Stale
        ));
    }

    #[test]
    fn svc_display_stale_only() {
        let svc = make_svc(
            ServiceState::Running,
            &[ProbeState::Green, ProbeState::Stale],
        );
        assert!(matches!(
            SvcDisplayState::from_service(&svc),
            SvcDisplayState::Stale
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
        let db = state.services.get_mut("db").unwrap();
        db.state = ServiceState::Running;
        for probe in db.probes.values_mut() {
            probe.state = ProbeState::Green;
        }
        assert_eq!(
            state.targets["t"].state(&state.services),
            TargetState::Green
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
        let state = RuntimeState::from_config(&config);
        // db is Stopped by default
        assert_eq!(state.targets["t"].state(&state.services), TargetState::Red);
    }

    #[test]
    fn target_state_stale_probe() {
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
        state.services.get_mut("db").unwrap().state = ServiceState::Running;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Stale;
        assert_eq!(
            state.targets["t"].state(&state.services),
            TargetState::Stale
        );
    }
}
