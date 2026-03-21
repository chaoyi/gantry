use indexmap::IndexMap;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

use crate::config::GantryConfig;
use crate::error::{GantryError, Result};
use crate::model::{ProbeRef, ProbeState, ServiceRuntime};

pub struct DependencyGraph {
    pub start_after_graph: DiGraph<String, ()>,
    pub depends_on_graph: DiGraph<String, ()>,
    svc_to_node: HashMap<String, NodeIndex>,
    probe_to_node: HashMap<String, NodeIndex>,
    pub topo_order: Vec<String>,
}

impl DependencyGraph {
    pub fn build(config: &GantryConfig) -> Result<Self> {
        let mut start_after_graph = DiGraph::new();
        let mut svc_to_node = HashMap::new();

        // Add service nodes
        for svc_name in config.services.keys() {
            let idx = start_after_graph.add_node(svc_name.clone());
            svc_to_node.insert(svc_name.clone(), idx);
        }

        // Add start_after edges: if service B has start_after [A.probe],
        // then A -> B (A must start before B)
        for (svc_name, svc_config) in &config.services {
            let to_idx = svc_to_node[svc_name];
            for probe_str in &svc_config.start_after {
                let probe_ref = ProbeRef::parse(probe_str).ok_or_else(|| {
                    GantryError::Validation(format!(
                        "invalid probe reference in start_after: {probe_str}"
                    ))
                })?;
                let from_idx = svc_to_node.get(&probe_ref.service).ok_or_else(|| {
                    GantryError::Validation(format!(
                        "service '{}' in start_after of '{}' does not exist",
                        probe_ref.service, svc_name
                    ))
                })?;
                start_after_graph.add_edge(*from_idx, to_idx, ());
            }
        }

        // Validate start_after is a DAG
        let topo_result = toposort(&start_after_graph, None);
        let topo_indices = topo_result.map_err(|cycle| {
            let node = &start_after_graph[cycle.node_id()];
            GantryError::Validation(format!(
                "start_after has a cycle involving service '{node}'"
            ))
        })?;
        let topo_order: Vec<String> = topo_indices
            .iter()
            .map(|idx| start_after_graph[*idx].clone())
            .collect();

        // Build depends_on graph (probe -> probe)
        let mut depends_on_graph = DiGraph::new();
        let mut probe_to_node = HashMap::new();

        for (svc_name, svc_config) in &config.services {
            for probe_name in svc_config.probes.keys() {
                let key = format!("{svc_name}.{probe_name}");
                let idx = depends_on_graph.add_node(key.clone());
                probe_to_node.insert(key, idx);
            }
        }

        for (svc_name, svc_config) in &config.services {
            for (probe_name, probe_entry) in &svc_config.probes {
                let to_key = format!("{svc_name}.{probe_name}");
                let to_idx = probe_to_node[&to_key];
                for dep_str in &probe_entry.depends_on {
                    let from_idx = probe_to_node.get(dep_str).ok_or_else(|| {
                        GantryError::Validation(format!(
                            "probe '{dep_str}' in depends_on of '{to_key}' does not exist"
                        ))
                    })?;
                    depends_on_graph.add_edge(*from_idx, to_idx, ());
                }
            }
        }

        // Validate all target probe references
        for (tgt_name, tgt_config) in &config.targets {
            for probe_str in &tgt_config.probes {
                if !probe_to_node.contains_key(probe_str) {
                    return Err(GantryError::Validation(format!(
                        "target '{tgt_name}' references unknown probe '{probe_str}'"
                    )));
                }
            }
            for dep_tgt in &tgt_config.depends_on {
                if !config.targets.contains_key(dep_tgt) {
                    return Err(GantryError::Validation(format!(
                        "target '{tgt_name}' depends on unknown target '{dep_tgt}'"
                    )));
                }
            }
        }

        Ok(Self {
            start_after_graph,
            depends_on_graph,
            svc_to_node,
            probe_to_node,
            topo_order,
        })
    }

    /// Get services in topological order, filtered to only those in the given set.
    pub fn topo_filtered(&self, service_names: &[String]) -> Vec<String> {
        self.topo_order
            .iter()
            .filter(|s| service_names.contains(s))
            .cloned()
            .collect()
    }

    /// Topological order of probes via depends_on graph.
    /// Returns probe keys (e.g., "db.port", "db.ready", "app.http") in dependency order:
    /// a probe's deps always come before the probe itself.
    /// Cycles are handled by returning probes in whatever order petgraph gives.
    pub fn probe_topo_order(&self) -> Vec<String> {
        match toposort(&self.depends_on_graph, None) {
            Ok(order) => order
                .into_iter()
                .map(|idx| self.depends_on_graph[idx].clone())
                .collect(),
            Err(_) => {
                // Cycle — return all probes in arbitrary order
                self.probe_to_node.keys().cloned().collect()
            }
        }
    }

    /// Group services into topo levels. Services in the same level have no
    /// start_after dependencies on each other and can be started in parallel.
    pub fn topo_levels(&self, service_names: &[String]) -> Vec<Vec<String>> {
        let mut levels: Vec<Vec<String>> = Vec::new();
        let mut assigned: HashMap<String, usize> = HashMap::new();

        for svc_name in &self.topo_order {
            if !service_names.contains(svc_name) {
                continue;
            }
            // Find the max level of all dependencies
            let node = self.svc_to_node[svc_name];
            let max_dep_level = self
                .start_after_graph
                .neighbors_directed(node, petgraph::Direction::Incoming)
                .filter_map(|dep_idx| {
                    let dep_name = &self.start_after_graph[dep_idx];
                    assigned.get(dep_name).copied()
                })
                .max();

            let my_level = match max_dep_level {
                Some(l) => l + 1,
                None => 0,
            };

            while levels.len() <= my_level {
                levels.push(Vec::new());
            }
            levels[my_level].push(svc_name.clone());
            assigned.insert(svc_name.clone(), my_level);
        }

        levels
    }

    /// Get all probes that depend on the given probe (reverse depends_on).
    pub fn reverse_depends_on(&self, probe_key: &str) -> Vec<String> {
        let Some(&node_idx) = self.probe_to_node.get(probe_key) else {
            return Vec::new();
        };
        self.depends_on_graph
            .neighbors_directed(node_idx, petgraph::Direction::Outgoing)
            .map(|idx| self.depends_on_graph[idx].clone())
            .collect()
    }

    /// Propagate state change downstream through depends_on edges.
    /// Checks the source probe's state: Red source → dependents go Red, Stale source → dependents go Stale.
    /// Only affects Green dependents (already-Red/Stale probes are not changed).
    pub fn propagate_staleness(
        &self,
        probe_key: &str,
        services: &mut IndexMap<String, ServiceRuntime>,
        changes: &mut Vec<(ProbeRef, ProbeState, ProbeState)>,
    ) {
        // Determine propagated state from source
        let source_state = ProbeRef::parse(probe_key).and_then(|pr| {
            services
                .get(&pr.service)?
                .probes
                .get(&pr.probe)
                .map(|p| p.state)
        });
        let propagate_as = match source_state {
            Some(ProbeState::Red) => ProbeState::Red,
            _ => ProbeState::Stale,
        };

        let dependents = self.reverse_depends_on(probe_key);
        for dep_key in dependents {
            if let Some(probe_ref) = ProbeRef::parse(&dep_key)
                && let Some(svc) = services.get_mut(&probe_ref.service)
                && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
                && (probe.state == ProbeState::Green
                    || (propagate_as == ProbeState::Red && probe.state == ProbeState::Stale))
            {
                let prev = probe.state;
                probe.prev_state = Some(prev);
                probe.state = propagate_as;
                tracing::debug!("[{dep_key}] {:?} (depends on {probe_key})", propagate_as);
                changes.push((probe_ref, propagate_as, prev));
                self.propagate_staleness(&dep_key, services, changes);
            }
        }
    }

    /// Mark a probe as red and propagate staleness downstream.
    /// Returns all (probe_ref, new_state, prev_state) changes.
    pub fn mark_red(
        &self,
        probe_ref: &ProbeRef,
        services: &mut IndexMap<String, ServiceRuntime>,
        changes: &mut Vec<(ProbeRef, ProbeState, ProbeState)>,
    ) {
        let probe_key = probe_ref.to_string();
        if let Some(svc) = services.get_mut(&probe_ref.service)
            && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
        {
            let prev = probe.state;
            probe.prev_state = Some(prev);
            probe.state = ProbeState::Red;
            changes.push((probe_ref.clone(), ProbeState::Red, prev));
        }
        self.propagate_staleness(&probe_key, services, changes);
    }

    /// Recovery propagation: when a probe goes green, mark non-stale reverse-deps as stale.
    /// But only if the dependent doesn't still have other Red deps (keep it Red if so).
    pub fn propagate_recovery(
        &self,
        probe_key: &str,
        services: &mut IndexMap<String, ServiceRuntime>,
        changes: &mut Vec<(ProbeRef, ProbeState, ProbeState)>,
    ) {
        let dependents = self.reverse_depends_on(probe_key);
        for dep_key in dependents {
            if let Some(probe_ref) = ProbeRef::parse(&dep_key) {
                // Check if the dependent still has other Red deps — if so, keep it Red
                let has_other_red = {
                    let probe = &services[&probe_ref.service].probes[&probe_ref.probe];
                    probe.depends_on.iter().any(|dep| {
                        dep.to_string() != probe_key && // skip the recovered dep
                        services.get(&dep.service)
                            .and_then(|s| s.probes.get(&dep.probe))
                            .is_some_and(|p| p.state == ProbeState::Red)
                    })
                };
                if has_other_red {
                    continue; // keep it Red — not all deps recovered
                }

                if let Some(svc) = services.get_mut(&probe_ref.service)
                    && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
                    && probe.state != ProbeState::Stale
                {
                    let prev = probe.state;
                    probe.prev_state = Some(prev);
                    probe.state = ProbeState::Stale;
                    tracing::debug!("[{dep_key}] stale (dependency {probe_key} recovered)");
                    changes.push((probe_ref, ProbeState::Stale, prev));
                    self.propagate_recovery(&dep_key, services, changes);
                }
            }
        }
    }

    /// Flatten a target: collect all transitive probes including from depends_on targets.
    /// Flatten a target: collect all transitive probes including from
    /// depends_on targets AND probe depends_on chains.
    pub fn flatten_target(&self, target_name: &str, config: &GantryConfig) -> Vec<ProbeRef> {
        let mut probes = Vec::new();
        let mut visited_targets = std::collections::HashSet::new();
        // Step 1: collect probes from target definitions
        self.flatten_target_recursive(target_name, config, &mut probes, &mut visited_targets);
        // Step 2: walk probe depends_on chains to include all transitive deps
        let mut visited_probes = std::collections::HashSet::new();
        let mut i = 0;
        while i < probes.len() {
            let pr = probes[i].clone();
            let key = pr.to_string();
            if visited_probes.insert(key.clone()) {
                // Look up this probe's depends_on
                if let Some(svc_config) = config.services.get(&pr.service)
                    && let Some(probe_config) = svc_config.probes.get(&pr.probe)
                {
                    for dep_str in &probe_config.depends_on {
                        if let Some(dep_pr) = ProbeRef::parse(dep_str)
                            && !probes.contains(&dep_pr)
                        {
                            probes.push(dep_pr);
                        }
                    }
                }
            }
            i += 1;
        }
        probes
    }

    fn flatten_target_recursive(
        &self,
        target_name: &str,
        config: &GantryConfig,
        probes: &mut Vec<ProbeRef>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if !visited.insert(target_name.to_string()) {
            return;
        }
        if let Some(tgt) = config.targets.get(target_name) {
            for probe_str in &tgt.probes {
                if let Some(pr) = ProbeRef::parse(probe_str)
                    && !probes.contains(&pr)
                {
                    probes.push(pr);
                }
            }
            for dep_tgt in &tgt.depends_on {
                self.flatten_target_recursive(dep_tgt, config, probes, visited);
            }
        }
    }

    /// Collect all transitive probes for satisfaction: given a set of direct probes,
    /// walk depends_on to find all probes that must be green.
    pub fn transitive_deps(&self, probe_ref: &ProbeRef) -> Vec<String> {
        let probe_key = probe_ref.to_string();
        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();
        self.transitive_deps_recursive(&probe_key, &mut result, &mut visited);
        result
    }

    fn transitive_deps_recursive(
        &self,
        probe_key: &str,
        result: &mut Vec<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if !visited.insert(probe_key.to_string()) {
            return;
        }
        let Some(&node_idx) = self.probe_to_node.get(probe_key) else {
            return;
        };
        // depends_on edges go from dependency -> dependent,
        // so incoming edges are this probe's dependencies
        for dep_idx in self
            .depends_on_graph
            .neighbors_directed(node_idx, petgraph::Direction::Incoming)
        {
            let dep_key = &self.depends_on_graph[dep_idx];
            if !result.contains(dep_key) {
                result.push(dep_key.clone());
            }
            self.transitive_deps_recursive(dep_key, result, visited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceState;

    fn test_config() -> GantryConfig {
        let yaml = r#"
services:
  db:
    container: demo-db-1
    probes:
      port:
        probe:
          type: tcp
          port: 5432
          timeout: 10s
      ready:
        probe:
          type: meta
        depends_on: [db.port]
  app:
    container: demo-app-1
    start_after: [db.ready]
    probes:
      http:
        probe:
          type: tcp
          port: 8080
          timeout: 10s
        depends_on: [db.ready]
      ready:
        probe:
          type: meta
        depends_on: [app.http]
targets:
  integration:
    probes: [app.ready]
"#;
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn build_graph_valid() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        assert_eq!(graph.topo_order, vec!["db", "app"]);
    }

    #[test]
    fn detect_start_after_cycle() {
        let yaml = r#"
services:
  a:
    container: a
    start_after: [b.ready]
    probes:
      ready:
        probe:
          type: meta
  b:
    container: b
    start_after: [a.ready]
    probes:
      ready:
        probe:
          type: meta
targets:
  t:
    probes: [a.ready]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let result = DependencyGraph::build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn flatten_target_with_deps() {
        let yaml = r#"
services:
  db:
    container: db
    probes:
      ready:
        probe:
          type: meta
  app:
    container: app
    probes:
      ready:
        probe:
          type: meta
targets:
  db-ready:
    probes: [db.ready]
  integration:
    probes: [app.ready]
    depends_on: [db-ready]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let graph = DependencyGraph::build(&config).unwrap();
        let probes = graph.flatten_target("integration", &config);
        assert_eq!(probes.len(), 2);
        assert!(probes.contains(&ProbeRef::new("app", "ready")));
        assert!(probes.contains(&ProbeRef::new("db", "ready")));
    }

    #[test]
    fn staleness_propagation() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything to green
        for svc in state.services.values_mut() {
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Mark db.port as red
        let db_port = ProbeRef::new("db", "port");
        let mut changes = Vec::new();
        graph.mark_red(&db_port, &mut state.services, &mut changes);

        // db.port should be red
        assert_eq!(state.services["db"].probes["port"].state, ProbeState::Red);
        // db.ready depends on db.port -> red (source is red)
        assert_eq!(state.services["db"].probes["ready"].state, ProbeState::Red);
        // app.http depends on db.ready -> red (transitive from red source)
        assert_eq!(state.services["app"].probes["http"].state, ProbeState::Red);
        // app.ready depends on app.http -> red
        assert_eq!(state.services["app"].probes["ready"].state, ProbeState::Red);
    }

    #[test]
    fn recovery_propagation() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything to green
        for svc in state.services.values_mut() {
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Mark db.port as red (simulates stop) → propagates stale downstream
        let db_port = ProbeRef::new("db", "port");
        let mut changes = Vec::new();
        graph.mark_red(&db_port, &mut state.services, &mut changes);
        assert_eq!(state.services["app"].probes["http"].state, ProbeState::Red);

        // Simulate: app.http reprobed while dep is red → stays red
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red;

        // Now db.port recovers to green → recovery propagation
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Green;
        let mut recovery = Vec::new();
        graph.propagate_recovery("db.port", &mut state.services, &mut recovery);

        // db.ready was stale → now stale (no change, skipped)
        // But it was stale from mark_red, so propagate_recovery skips it (already stale)

        // Reset app.http to red (simulating it was reprobed and failed)
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red;

        // Simulate db.ready also recovers
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("ready")
            .unwrap()
            .state = ProbeState::Green;
        let mut recovery2 = Vec::new();
        graph.propagate_recovery("db.ready", &mut state.services, &mut recovery2);

        // app.http was red → now stale (recovery propagation)
        assert_eq!(
            state.services["app"].probes["http"].state,
            ProbeState::Stale
        );
        assert!(
            recovery2
                .iter()
                .any(|(pr, new, _)| pr.to_string() == "app.http" && *new == ProbeState::Stale)
        );
    }

    #[test]
    fn recovery_propagation_green_dependents() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Everything green
        for svc in state.services.values_mut() {
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // db.port goes red then recovers
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Green;
        let mut changes = Vec::new();
        graph.propagate_recovery("db.port", &mut state.services, &mut changes);

        // db.ready was green → now stale (green dependents also staled)
        assert_eq!(
            state.services["db"].probes["ready"].state,
            ProbeState::Stale
        );
        // app.http was green → now stale (transitive)
        assert_eq!(
            state.services["app"].probes["http"].state,
            ProbeState::Stale
        );
    }

    #[test]
    fn topo_levels_parallel() {
        let yaml = r#"
services:
  db:
    container: db
    probes:
      ready:
        probe:
          type: meta
  redis:
    container: redis
    probes:
      ready:
        probe:
          type: meta
  app:
    container: app
    start_after: [db.ready, redis.ready]
    probes:
      ready:
        probe:
          type: meta
  worker:
    container: worker
    start_after: [db.ready]
    probes:
      ready:
        probe:
          type: meta
targets:
  full:
    probes: [app.ready, worker.ready]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let graph = DependencyGraph::build(&config).unwrap();
        let all_svcs = vec![
            "db".to_string(),
            "redis".to_string(),
            "app".to_string(),
            "worker".to_string(),
        ];
        let levels = graph.topo_levels(&all_svcs);

        // Level 0: db, redis (no deps, parallel)
        assert_eq!(levels.len(), 2);
        assert!(levels[0].contains(&"db".to_string()));
        assert!(levels[0].contains(&"redis".to_string()));
        // Level 1: app, worker (both depend on level 0)
        assert!(levels[1].contains(&"app".to_string()));
        assert!(levels[1].contains(&"worker".to_string()));
    }

    #[test]
    fn test_probe_topo_order_deps_before_dependents() {
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let order = graph.probe_topo_order();

        // Helper: position in topo order
        let pos = |key: &str| order.iter().position(|k| k == key).unwrap_or(usize::MAX);

        // db.port before db.accepting (accepting depends on port)
        assert!(
            pos("db.port") < pos("db.accepting"),
            "db.port should come before db.accepting"
        );
        // db.port before db.ready (ready depends on port + accepting)
        assert!(pos("db.port") < pos("db.ready"));
        // db.ready before web.http (web.http depends on db.ready)
        assert!(
            pos("db.ready") < pos("web.http"),
            "db.ready should come before web.http"
        );
        // db.ready before app.init (app.init depends on db.ready)
        assert!(
            pos("db.ready") < pos("app.init"),
            "db.ready should come before app.init"
        );
        // redis.ready before app.http (app.http depends on redis.ready)
        assert!(
            pos("redis.ready") < pos("app.http"),
            "redis.ready should come before app.http"
        );
        // app.init before app.http (app.http depends on app.init)
        assert!(
            pos("app.init") < pos("app.http"),
            "app.init should come before app.http"
        );
        // web.http before web.ready
        assert!(pos("web.http") < pos("web.ready"));
        // app.http before app.ready
        assert!(pos("app.http") < pos("app.ready"));
    }

    #[test]
    fn propagate_staleness_only_from_non_green() {
        // Callers must only call propagate_staleness for probes that are NOT green.
        // This test verifies the converge pattern: after restart, only propagate
        // from probes that are still stale/red, not from green ones.
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Everything green and running
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Simulate: web restarted, web.http is now green, web.ready is green.
        // Only propagate from probes that are NOT green (none in this case).
        let non_green: Vec<String> = state.services["web"]
            .probes
            .iter()
            .filter(|(_, probe)| probe.state != ProbeState::Green)
            .map(|(name, _)| format!("web.{name}"))
            .collect();
        let mut changes = Vec::new();
        for probe_key in &non_green {
            graph.propagate_staleness(probe_key, &mut state.services, &mut changes);
        }

        // Nothing should change — all probes are green, no propagation needed.
        assert!(changes.is_empty());
        assert_eq!(
            state.services["web"].probes["ready"].state,
            ProbeState::Green
        );
    }

    #[test]
    fn propagate_staleness_from_red_probe_stales_dependents() {
        // When a probe goes red, its green dependents should go stale.
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // web.http goes red — propagate staleness
        state
            .services
            .get_mut("web")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red;
        let mut changes = Vec::new();
        graph.propagate_staleness("web.http", &mut state.services, &mut changes);

        // web.ready (depends on web.http, was green) → now RED (source is Red)
        assert_eq!(state.services["web"].probes["ready"].state, ProbeState::Red);
        assert!(
            changes
                .iter()
                .any(|(pr, new, _)| pr.to_string() == "web.ready" && *new == ProbeState::Red)
        );
    }

    fn demo_config() -> GantryConfig {
        // Note: uses serde_yaml then manually calls auto_generate + topo_sort
        // to match what GantryConfig::load() does
        let yaml = r#"
services:
  db:
    container: db
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
      accepting:
        probe: { type: log, success: "ready", timeout: 10s }
        depends_on: [db.port]
  redis:
    container: redis
    probes:
      port:
        probe: { type: tcp, port: 6379, timeout: 10s }
  web:
    container: web
    start_after: [db.ready]
    probes:
      http:
        probe: { type: tcp, port: 80, timeout: 10s }
        depends_on: [db.ready]
  app:
    container: app
    start_after: [db.ready, redis.ready]
    probes:
      init:
        probe: { type: log, success: "connected", timeout: 10s }
        depends_on: [db.ready]
      http:
        probe: { type: tcp, port: 8080, timeout: 10s }
        depends_on: [app.init, redis.ready]
targets:
  full:
    probes: [app.ready, web.ready]
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.auto_generate_ready_probes();
        config.topo_sort_probes();
        config
    }

    #[test]
    fn reverse_depends_on_correct() {
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();

        // db.port → [db.accepting]
        let rev = graph.reverse_depends_on("db.port");
        assert!(rev.contains(&"db.accepting".to_string()));
        assert!(!rev.contains(&"web.http".to_string()));

        // db.ready → [web.http, app.init]
        let rev = graph.reverse_depends_on("db.ready");
        assert!(rev.contains(&"web.http".to_string()));
        assert!(rev.contains(&"app.init".to_string()));
        assert!(!rev.contains(&"app.http".to_string())); // app.http depends on app.init, not db.ready

        // redis.ready → [app.http]
        let rev = graph.reverse_depends_on("redis.ready");
        assert!(rev.contains(&"app.http".to_string()));
        assert!(!rev.contains(&"web.http".to_string())); // web doesn't depend on redis

        // app.http → [app.ready]
        let rev = graph.reverse_depends_on("app.http");
        assert!(rev.contains(&"app.ready".to_string()));
        assert!(!rev.contains(&"web.ready".to_string())); // web.ready doesn't depend on app.http

        // web.http → [web.ready]
        let rev = graph.reverse_depends_on("web.http");
        assert!(rev.contains(&"web.ready".to_string()));
    }

    #[test]
    fn recovery_propagation_does_not_cross_services_incorrectly() {
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything green
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Mark web.http as stale (simulating it was red then its dep recovered)
        state
            .services
            .get_mut("web")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red;

        // web.http recovers to green → propagate_recovery
        state
            .services
            .get_mut("web")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Green;
        let mut changes = Vec::new();
        graph.propagate_recovery("web.http", &mut state.services, &mut changes);

        // web.ready should be stale (depends on web.http)
        assert_eq!(
            state.services["web"].probes["ready"].state,
            ProbeState::Stale
        );

        // app.ready should NOT be affected (doesn't depend on web.http)
        assert_eq!(
            state.services["app"].probes["ready"].state,
            ProbeState::Green
        );
        // app.http should NOT be affected
        assert_eq!(
            state.services["app"].probes["http"].state,
            ProbeState::Green
        );
    }

    #[test]
    fn all_stale_then_resolve_in_topo_order() {
        // Simulates reprobe-all: everything stale, probes all pass, resolve in topo order
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything running + stale (simulates force reprobe-all)
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Stale;
            }
        }

        // Simulate: all probes pass, resolve in topo order
        // Topo levels: [db, redis] then [web, app]
        let all_svcs: Vec<String> = state.services.keys().cloned().collect();
        let levels = graph.topo_levels(&all_svcs);

        for level in &levels {
            for svc_name in level {
                let svc = &state.services[svc_name];
                let probe_names: Vec<String> = svc.probes.keys().cloned().collect();
                for probe_name in &probe_names {
                    let probe = &state.services[svc_name].probes[probe_name];
                    if matches!(probe.probe_config, crate::config::ProbeConfig::Meta) {
                        continue; // Meta probes resolved by separate step
                    }
                    // "Probe passes" — check deps to determine state
                    let deps_all_green = probe.depends_on.iter().all(|dep| {
                        state
                            .services
                            .get(&dep.service)
                            .and_then(|s| s.probes.get(&dep.probe))
                            .is_some_and(|p| p.state == ProbeState::Green)
                    });
                    let new_state = if deps_all_green {
                        ProbeState::Green
                    } else {
                        ProbeState::Stale
                    };
                    state
                        .services
                        .get_mut(svc_name)
                        .unwrap()
                        .probes
                        .get_mut(probe_name)
                        .unwrap()
                        .state = new_state;

                    // Recovery propagation if went green
                    if new_state == ProbeState::Green {
                        let mut changes = Vec::new();
                        graph.propagate_recovery(
                            &format!("{svc_name}.{probe_name}"),
                            &mut state.services,
                            &mut changes,
                        );
                    }
                }
                // Resolve meta probes
                for probe_name in &probe_names {
                    let probe = &state.services[svc_name].probes[probe_name];
                    if !matches!(probe.probe_config, crate::config::ProbeConfig::Meta) {
                        continue;
                    }
                    let satisfied = crate::probe::meta::is_satisfied(
                        &ProbeRef::new(svc_name, probe_name),
                        &state.services,
                    );
                    let new_state = if satisfied {
                        ProbeState::Green
                    } else {
                        ProbeState::Red
                    };
                    state
                        .services
                        .get_mut(svc_name)
                        .unwrap()
                        .probes
                        .get_mut(probe_name)
                        .unwrap()
                        .state = new_state;
                    if new_state == ProbeState::Green {
                        let mut changes = Vec::new();
                        graph.propagate_recovery(
                            &format!("{svc_name}.{probe_name}"),
                            &mut state.services,
                            &mut changes,
                        );
                    }
                }
            }
        }

        // ALL probes should be green
        for (svc_name, svc) in &state.services {
            for (probe_name, probe) in &svc.probes {
                assert_eq!(
                    probe.state,
                    ProbeState::Green,
                    "{svc_name}.{probe_name} should be green but is {:?}",
                    probe.state
                );
            }
        }
    }

    #[test]
    fn red_propagates_to_stale_dependents() {
        // If app.http is stale and db.ready goes red, app.http should go red (not stay stale)
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      ready:
        probe: { type: tcp, port: 5432, timeout: 10s }
  app:
    container: app-1
    probes:
      http:
        probe: { type: tcp, port: 8080, timeout: 10s }
        depends_on: [db.ready]
targets:
  full:
    probes: [app.http]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        for (_, svc) in state.services.iter_mut() {
            svc.state = ServiceState::Running;
        }
        // Set app.http to stale, db.ready to green
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Stale;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("ready")
            .unwrap()
            .state = ProbeState::Green;

        // Now db.ready goes red → propagate
        let mut changes = Vec::new();
        graph.mark_red(
            &ProbeRef::new("db", "ready"),
            &mut state.services,
            &mut changes,
        );

        // app.http was stale, dep went red → should now be red
        assert_eq!(
            state.services["app"].probes["http"].state,
            ProbeState::Red,
            "stale probe with red dep should become red"
        );
    }

    #[test]
    fn recovery_blocked_by_other_red_dep() {
        // app.http depends on db.ready AND redis.port.
        // db.ready recovers, but redis.port is still red.
        // app.http should stay red (not go stale).
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      ready:
        probe: { type: tcp, port: 5432, timeout: 10s }
  redis:
    container: redis-1
    probes:
      port:
        probe: { type: tcp, port: 6379, timeout: 10s }
  app:
    container: app-1
    probes:
      http:
        probe: { type: tcp, port: 8080, timeout: 10s }
        depends_on: [db.ready, redis.port]
targets:
  full:
    probes: [app.http]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        for (_, svc) in state.services.iter_mut() {
            svc.state = ServiceState::Running;
        }
        // All red initially
        for (_, svc) in state.services.iter_mut() {
            for (_, probe) in svc.probes.iter_mut() {
                probe.state = ProbeState::Red;
            }
        }

        // db.ready recovers to green
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("ready")
            .unwrap()
            .state = ProbeState::Green;
        let mut changes = Vec::new();
        graph.propagate_recovery("db.ready", &mut state.services, &mut changes);

        // app.http should stay red — redis.port is still red
        assert_eq!(
            state.services["app"].probes["http"].state,
            ProbeState::Red,
            "should stay red when another dep is still red"
        );
    }

    #[test]
    fn target_requires_dependent_targets_green() {
        let yaml = r#"
services:
  db:
    container: db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 10s }
  app:
    container: app-1
    probes:
      http:
        probe: { type: tcp, port: 8080, timeout: 10s }
targets:
  infra:
    probes: [db.port]
  full:
    probes: [app.http]
    depends_on: [infra]
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.auto_generate_ready_probes();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        for (tgt_name, tgt) in state.targets.iter_mut() {
            tgt.transitive_probes = graph.flatten_target(tgt_name, &config);
        }
        for (_, svc) in state.services.iter_mut() {
            svc.state = ServiceState::Running;
        }

        // app.http green, but db.port red
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Green;
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("ready")
            .unwrap()
            .state = ProbeState::Green;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Red;

        // full target should not be green (infra dep is red)
        let full_state = state.targets["full"].state(&state.services);
        assert_ne!(
            full_state,
            crate::model::TargetState::Green,
            "target should not be green when dependent target's probes are red"
        );
    }
}
