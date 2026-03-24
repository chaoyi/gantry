use crate::model::{ProbeRef, ServiceRuntime};
use indexmap::IndexMap;

/// A meta probe is satisfied when all its depends_on are satisfied.
pub fn is_satisfied(probe_ref: &ProbeRef, services: &IndexMap<String, ServiceRuntime>) -> bool {
    let Some(svc) = services.get(&probe_ref.service) else {
        return false;
    };
    let Some(probe) = svc.probes.get(&probe_ref.probe) else {
        return false;
    };
    for dep in &probe.depends_on {
        if !is_probe_satisfied(dep, services) {
            return false;
        }
    }
    true
}

pub fn is_probe_satisfied(
    probe_ref: &ProbeRef,
    services: &IndexMap<String, ServiceRuntime>,
) -> bool {
    let Some(svc) = services.get(&probe_ref.service) else {
        return false;
    };
    if svc.state != crate::model::ServiceState::Running {
        return false;
    }
    let Some(probe) = svc.probes.get(&probe_ref.probe) else {
        return false;
    };
    if !probe.state.is_green() {
        return false;
    }
    // Check transitive deps
    for dep in &probe.depends_on {
        if !is_probe_satisfied(dep, services) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProbeConfig;
    use crate::model::{ProbeRuntime, ProbeState, ServiceState};

    fn make_probe(name: &str, svc: &str, state: ProbeState, deps: Vec<ProbeRef>) -> ProbeRuntime {
        ProbeRuntime {
            probe_ref: ProbeRef::new(svc, name),
            state: state.clone(),
            prev_color: None,
            probe_config: ProbeConfig::Meta,
            depends_on: deps,
            last_probe_ms: None,
            last_error: None,
        }
    }

    fn make_svc(name: &str, state: ServiceState, probes: Vec<ProbeRuntime>) -> ServiceRuntime {
        let mut probe_map = IndexMap::new();
        for p in probes {
            probe_map.insert(p.probe_ref.probe.clone(), p);
        }
        ServiceRuntime {
            name: name.into(),
            container: format!("{name}-1"),
            state,
            probes: probe_map,
            start_after: Vec::new(),
            restart_on_fail: true,
            generation: 0,
            log_since: 0,
            last_emitted_display: None,
        }
    }

    #[test]
    fn meta_satisfied_all_deps_green() {
        let svc = make_svc(
            "svc",
            ServiceState::Running,
            vec![
                make_probe("port", "svc", ProbeState::Green, vec![]),
                make_probe(
                    "http",
                    "svc",
                    ProbeState::Green,
                    vec![ProbeRef::new("svc", "port")],
                ),
                make_probe(
                    "ready",
                    "svc",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![ProbeRef::new("svc", "port"), ProbeRef::new("svc", "http")],
                ),
            ],
        );
        let mut services = IndexMap::new();
        services.insert("svc".into(), svc);
        assert!(is_satisfied(&ProbeRef::new("svc", "ready"), &services));
    }

    #[test]
    fn meta_not_satisfied_dep_red() {
        let svc = make_svc(
            "svc",
            ServiceState::Running,
            vec![
                make_probe(
                    "port",
                    "svc",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![],
                ),
                make_probe(
                    "ready",
                    "svc",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![ProbeRef::new("svc", "port")],
                ),
            ],
        );
        let mut services = IndexMap::new();
        services.insert("svc".into(), svc);
        assert!(!is_satisfied(&ProbeRef::new("svc", "ready"), &services));
    }

    #[test]
    fn meta_not_satisfied_dep_stale() {
        let svc = make_svc(
            "svc",
            ServiceState::Running,
            vec![
                make_probe(
                    "port",
                    "svc",
                    ProbeState::Stale(crate::model::StaleReason::Reprobing),
                    vec![],
                ),
                make_probe(
                    "ready",
                    "svc",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![ProbeRef::new("svc", "port")],
                ),
            ],
        );
        let mut services = IndexMap::new();
        services.insert("svc".into(), svc);
        assert!(!is_satisfied(&ProbeRef::new("svc", "ready"), &services));
    }

    #[test]
    fn meta_not_satisfied_service_stopped() {
        let svc = make_svc(
            "svc",
            ServiceState::Stopped,
            vec![
                make_probe("port", "svc", ProbeState::Green, vec![]),
                make_probe(
                    "ready",
                    "svc",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![ProbeRef::new("svc", "port")],
                ),
            ],
        );
        let mut services = IndexMap::new();
        services.insert("svc".into(), svc);
        assert!(!is_satisfied(&ProbeRef::new("svc", "ready"), &services));
    }

    #[test]
    fn meta_cross_service_dep() {
        let db = make_svc(
            "db",
            ServiceState::Running,
            vec![make_probe("port", "db", ProbeState::Green, vec![])],
        );
        let app = make_svc(
            "app",
            ServiceState::Running,
            vec![
                make_probe(
                    "http",
                    "app",
                    ProbeState::Green,
                    vec![ProbeRef::new("db", "port")],
                ),
                make_probe(
                    "ready",
                    "app",
                    ProbeState::Red(crate::model::RedReason::Stopped),
                    vec![ProbeRef::new("app", "http")],
                ),
            ],
        );
        let mut services = IndexMap::new();
        services.insert("db".into(), db);
        services.insert("app".into(), app);
        assert!(is_satisfied(&ProbeRef::new("app", "ready"), &services));
    }
}
