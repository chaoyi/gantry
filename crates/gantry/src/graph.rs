use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::IndexMap;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::Dfs;

use crate::config::GantryConfig;
use crate::error::{GantryError, Result};
use crate::model::{ProbeRef, ProbeState, ServiceRuntime, ServiceState};

pub struct DependencyGraph {
    pub start_after_graph: DiGraph<String, ()>,
    pub depends_on_graph: DiGraph<String, ()>,
    svc_to_node: HashMap<String, NodeIndex>,
    probe_to_node: HashMap<String, NodeIndex>,
    pub topo_order: Vec<String>,
    pub probe_topo_order: Vec<String>,
    target_graph: DiGraph<String, ()>,
    tgt_to_node: HashMap<String, NodeIndex>,
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

        // Validate depends_on is a DAG (no cycles) and compute topo order
        let probe_topo_order = match toposort(&depends_on_graph, None) {
            Ok(order) => order
                .into_iter()
                .map(|idx| depends_on_graph[idx].clone())
                .collect(),
            Err(cycle) => {
                let node = &depends_on_graph[cycle.node_id()];
                return Err(GantryError::Validation(format!(
                    "depends_on has a cycle involving probe '{node}'"
                )));
            }
        };

        // Build and validate target depends_on DAG (no cycles) via petgraph
        let mut target_graph = DiGraph::<String, ()>::new();
        let mut tgt_to_node: HashMap<String, NodeIndex> = HashMap::new();
        for tgt_name in config.targets.keys() {
            let idx = target_graph.add_node(tgt_name.clone());
            tgt_to_node.insert(tgt_name.clone(), idx);
        }
        for (tgt_name, tgt_config) in &config.targets {
            let to_idx = tgt_to_node[tgt_name];
            for dep in &tgt_config.depends_on {
                if let Some(&from_idx) = tgt_to_node.get(dep) {
                    target_graph.add_edge(from_idx, to_idx, ());
                }
            }
        }
        if let Err(cycle) = toposort(&target_graph, None) {
            let node = &target_graph[cycle.node_id()];
            return Err(GantryError::Validation(format!(
                "target depends_on has a cycle involving target '{node}'"
            )));
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
            probe_topo_order,
            target_graph,
            tgt_to_node,
        })
    }

    /// Topological order of probes via depends_on graph.
    /// Returns probe keys (e.g., "db.port", "db.ready", "app.http") in dependency order:
    /// a probe's deps always come before the probe itself.
    pub fn probe_topo_order(&self) -> &[String] {
        &self.probe_topo_order
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

    /// Propagate state change downstream through depends_on edges (BFS).
    /// Red source → dependents go Red; non-Red source → dependents go Pending.
    /// Only affects Green dependents (and Pending dependents when propagating Red).
    pub fn propagate_pending(
        &self,
        probe_key: &str,
        services: &mut IndexMap<String, ServiceRuntime>,
        changes: &mut Vec<(ProbeRef, ProbeState, ProbeState)>,
    ) {
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back(probe_key.to_string());
        visited.insert(probe_key.to_string());
        // Track probes that are "effectively red" for propagation purposes,
        // including stopped service probes whose state wasn't changed.
        let mut effectively_red = HashSet::new();
        if ProbeRef::parse(probe_key)
            .and_then(|pr| {
                services
                    .get(&pr.service)?
                    .probes
                    .get(&pr.probe)
                    .map(|p| p.state.is_red())
            })
            .unwrap_or(false)
        {
            effectively_red.insert(probe_key.to_string());
        }

        while let Some(current) = queue.pop_front() {
            let source_is_red = effectively_red.contains(&current);
            let current_ref = ProbeRef::parse(&current).unwrap();

            let Some(&node_idx) = self.probe_to_node.get(&current) else {
                continue;
            };
            for neighbor_idx in self
                .depends_on_graph
                .neighbors_directed(node_idx, petgraph::Direction::Outgoing)
            {
                let dep_key = &self.depends_on_graph[neighbor_idx];
                if !visited.insert(dep_key.clone()) {
                    continue;
                }
                let Some(probe_ref) = ProbeRef::parse(dep_key) else {
                    continue;
                };
                let Some(svc) = services.get_mut(&probe_ref.service) else {
                    continue;
                };
                let svc_running = matches!(svc.state, ServiceState::Running);
                if let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
                    && (probe.state.is_green() || (source_is_red && probe.state.is_pending()))
                {
                    if svc_running {
                        let prev = probe.state.clone();
                        probe.prev_color = Some(prev.color());
                        let propagate_as = if source_is_red {
                            ProbeState::Red(crate::model::RedReason::DepRed {
                                dep: current_ref.clone(),
                            })
                        } else {
                            ProbeState::Pending(crate::model::PendingReason::DepNotReady {
                                dep: current_ref.clone(),
                            })
                        };
                        probe.state = propagate_as.clone();
                        tracing::debug!(
                            "prb [{dep_key}] → {} (dep {current})",
                            propagate_as.as_str()
                        );
                        if propagate_as.is_red() {
                            effectively_red.insert(dep_key.clone());
                        }
                        changes.push((probe_ref, propagate_as, prev));
                    } else if source_is_red {
                        // Stopped service: don't change state but carry red-ness forward
                        effectively_red.insert(dep_key.clone());
                    }
                    // Always continue walking through stopped services
                    queue.push_back(dep_key.clone());
                }
            }
        }
    }

    /// Mark a probe as red and propagate pending downstream.
    /// Returns all (probe_ref, new_state, prev_color) changes.
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
            let prev = probe.state.clone();
            probe.prev_color = Some(prev.color());
            let new_state = ProbeState::Red(crate::model::RedReason::DepRed {
                dep: probe_ref.clone(),
            });
            probe.state = new_state.clone();
            changes.push((probe_ref.clone(), new_state, prev));
        }
        self.propagate_pending(&probe_key, services, changes);
    }

    /// Recovery propagation: when a probe goes green, walk reverse dependents
    /// and mark them Pending(DepRecovered) for re-checking.
    ///
    /// Green is transitive — if a dep fluctuated, all downstream must re-verify.
    /// Skip: Pending (already queued), ProbeFailed (own failure), stopped services.
    /// Recover: Green, Red(DepRed), Red(Stopped), Red(ContainerDied) on running services.
    /// has_other_red guard prevents recovery when other deps are still blocking.
    pub fn propagate_recovery(
        &self,
        probe_key: &str,
        services: &mut IndexMap<String, ServiceRuntime>,
        changes: &mut Vec<(ProbeRef, ProbeState, ProbeState)>,
    ) {
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back(probe_key.to_string());
        visited.insert(probe_key.to_string());

        while let Some(current) = queue.pop_front() {
            let current_ref = ProbeRef::parse(&current).unwrap();

            let Some(&node_idx) = self.probe_to_node.get(&current) else {
                continue;
            };
            for neighbor_idx in self
                .depends_on_graph
                .neighbors_directed(node_idx, petgraph::Direction::Outgoing)
            {
                let dep_key = &self.depends_on_graph[neighbor_idx];
                if !visited.insert(dep_key.clone()) {
                    continue;
                }
                let Some(probe_ref) = ProbeRef::parse(dep_key) else {
                    continue;
                };
                // Read probe state + deps without holding mutable borrow
                let (skip, svc_running, has_other_red) = {
                    let Some(svc) = services.get(&probe_ref.service) else {
                        continue;
                    };
                    let svc_running = matches!(svc.state, ServiceState::Running);
                    let Some(probe) = svc.probes.get(&probe_ref.probe) else {
                        continue;
                    };
                    // Skip Pending (already queued) and ProbeFailed (own failure)
                    let skip = probe.state.is_pending() || probe.state.is_probe_failed();
                    let has_other_red = probe.depends_on.iter().any(|dep| {
                        dep.to_string() != current
                            && services
                                .get(&dep.service)
                                .and_then(|s| s.probes.get(&dep.probe))
                                .is_some_and(|p| p.state.is_red())
                    });
                    (skip, svc_running, has_other_red)
                };

                if skip || !svc_running {
                    continue;
                }
                if has_other_red {
                    continue;
                }

                // Recover: mark Pending(DepRecovered)
                let svc = services.get_mut(&probe_ref.service).unwrap();
                let probe = svc.probes.get_mut(&probe_ref.probe).unwrap();
                let prev = probe.state.clone();

                // Invariant: recovery must not overwrite ProbeFailed
                debug_assert!(
                    !prev.is_probe_failed(),
                    "recovery propagation tried to overwrite ProbeFailed on {}",
                    dep_key
                );

                probe.prev_color = Some(prev.color());
                let new_state = ProbeState::Pending(crate::model::PendingReason::DepRecovered {
                    dep: current_ref.clone(),
                });
                probe.state = new_state.clone();
                tracing::debug!("prb [{dep_key}] → pending (dep {current} recovered)");
                changes.push((probe_ref, new_state, prev));

                queue.push_back(dep_key.clone());
            }
        }
    }

    /// Flatten a target: collect all transitive probes including from
    /// depends_on targets AND probe depends_on chains (via petgraph DFS).
    pub fn flatten_target(&self, target_name: &str, config: &GantryConfig) -> Vec<ProbeRef> {
        // Step 1: collect probes from target chain (BFS through target depends_on)
        let mut probes = Vec::new();
        if let Some(&start) = self.tgt_to_node.get(target_name) {
            let reversed = petgraph::visit::Reversed(&self.target_graph);
            let mut bfs = petgraph::visit::Bfs::new(&reversed, start);
            while let Some(node) = bfs.next(&reversed) {
                let tgt_name = &self.target_graph[node];
                if let Some(tgt) = config.targets.get(tgt_name) {
                    for probe_str in &tgt.probes {
                        if let Some(pr) = ProbeRef::parse(probe_str)
                            && !probes.contains(&pr)
                        {
                            probes.push(pr);
                        }
                    }
                }
            }
        }
        // Step 2: walk probe depends_on chains via petgraph DFS on incoming edges
        let reversed = petgraph::visit::Reversed(&self.depends_on_graph);
        let mut visited_probes = HashSet::new();
        let initial: Vec<ProbeRef> = probes.clone();
        for pr in &initial {
            let key = pr.to_string();
            if let Some(&node) = self.probe_to_node.get(&key) {
                let mut dfs = Dfs::new(&reversed, node);
                while let Some(dep_node) = dfs.next(&reversed) {
                    let dep_key = &self.depends_on_graph[dep_node];
                    if visited_probes.insert(dep_key.clone())
                        && let Some(dep_pr) = ProbeRef::parse(dep_key)
                        && !probes.contains(&dep_pr)
                    {
                        probes.push(dep_pr);
                    }
                }
            }
        }
        probes
    }

    /// Set initial probe states after docker inspect discovers running containers.
    /// Walks services in topo order (start_after). For running services whose
    /// start_after deps are all running, probes become Pending(Unchecked) so they
    /// get reprobed. Otherwise probes stay Red.
    pub fn initialize_probe_states(&self, services: &mut IndexMap<String, ServiceRuntime>) {
        use crate::model::{PendingReason, ProbeState, RedReason, ServiceState};

        // Walk in topo order so we process dependencies before dependents
        for svc_name in &self.topo_order {
            let svc_state = services[svc_name].state;
            if svc_state != ServiceState::Running {
                // Stopped/Crashed: probes stay Red(Stopped) as initialized
                continue;
            }

            // Check if all start_after deps are running
            let all_deps_running = {
                let svc = &services[svc_name];
                svc.start_after.iter().all(|dep_ref| {
                    services
                        .get(&dep_ref.service)
                        .is_some_and(|dep_svc| dep_svc.state == ServiceState::Running)
                })
            };

            if !all_deps_running {
                // Service is running but a dependency is stopped/crashed — probes stay Red
                continue;
            }

            // Mark probes as Pending(Unchecked), but respect probe-level depends_on:
            // if a probe's depends_on points to a red probe, keep it Red(DepRed).
            let probe_names: Vec<String> = services[svc_name].probes.keys().cloned().collect();
            for probe_name in probe_names {
                // Collect deps and find first red dep before mutating
                let first_red_dep = {
                    let probe = &services[svc_name].probes[&probe_name];
                    probe
                        .depends_on
                        .iter()
                        .find(|dep_ref| {
                            services
                                .get(&dep_ref.service)
                                .and_then(|s| s.probes.get(&dep_ref.probe))
                                .is_some_and(|p| p.state.is_red())
                        })
                        .cloned()
                };

                let probe = services
                    .get_mut(svc_name)
                    .unwrap()
                    .probes
                    .get_mut(&probe_name)
                    .unwrap();
                if let Some(dep) = first_red_dep {
                    probe.state = ProbeState::Red(RedReason::DepRed { dep });
                } else {
                    probe.state = ProbeState::Pending(PendingReason::Unchecked);
                }
            }
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
    fn detect_depends_on_cycle() {
        let yaml = r#"
services:
  a:
    container: a
    probes:
      x:
        probe: { type: tcp, port: 1, timeout: 1s }
        depends_on: [a.y]
      y:
        probe: { type: tcp, port: 2, timeout: 1s }
        depends_on: [a.x]
targets:
  t:
    probes: [a.x]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let err = DependencyGraph::build(&config).err().expect("should fail");
        assert!(
            err.to_string().contains("depends_on has a cycle"),
            "expected depends_on cycle error, got: {err}"
        );
    }

    #[test]
    fn detect_target_depends_on_cycle() {
        let yaml = r#"
services:
  a:
    container: a
    probes:
      ready:
        probe: { type: meta }
targets:
  t1:
    probes: [a.ready]
    depends_on: [t2]
  t2:
    probes: [a.ready]
    depends_on: [t1]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        let err = DependencyGraph::build(&config).err().expect("should fail");
        assert!(
            err.to_string().contains("target depends_on has a cycle"),
            "expected target cycle error, got: {err}"
        );
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
    fn pending_propagation() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything to running + green
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Mark db.port as red
        let db_port = ProbeRef::new("db", "port");
        let mut changes = Vec::new();
        graph.mark_red(&db_port, &mut state.services, &mut changes);

        // db.port should be red
        assert!(state.services["db"].probes["port"].state.is_red());
        // db.ready depends on db.port -> red (source is red)
        assert!(state.services["db"].probes["ready"].state.is_red());
        // app.http depends on db.ready -> red (transitive from red source)
        assert!(state.services["app"].probes["http"].state.is_red());
        // app.ready depends on app.http -> red
        assert!(state.services["app"].probes["ready"].state.is_red());
    }

    #[test]
    fn recovery_propagation() {
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything to running + green
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Mark db.port as red (simulates stop) → propagates pending downstream
        let db_port = ProbeRef::new("db", "port");
        let mut changes = Vec::new();
        graph.mark_red(&db_port, &mut state.services, &mut changes);
        assert!(state.services["app"].probes["http"].state.is_red());

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

        // Reset app.http to Red(DepRed) — simulating dep chain still blocked
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::DepRed {
            dep: ProbeRef::new("db", "ready"),
        });

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

        // app.http was red → now pending (recovery propagation)
        assert!(state.services["app"].probes["http"].state.is_pending());
        assert!(
            recovery2
                .iter()
                .any(|(pr, new, _)| pr.to_string() == "app.http" && new.is_pending())
        );
    }

    #[test]
    fn recovery_invalidates_green_dependents() {
        // Green is transitive — if a dep recovered, all downstream must re-verify.
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Everything running + green
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        let mut changes = Vec::new();
        graph.propagate_recovery("db.port", &mut state.services, &mut changes);

        // Green dependents become pending — dep fluctuated, must re-verify
        assert!(state.services["db"].probes["ready"].state.is_pending());
        assert!(state.services["app"].probes["http"].state.is_pending());
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
    fn propagate_pending_only_from_non_green() {
        // Callers must only call propagate_pending for probes that are NOT green.
        // This test verifies the converge pattern: after restart, only propagate
        // from probes that are still pending/red, not from green ones.
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
            .filter(|(_, probe)| !probe.state.is_green())
            .map(|(name, _)| format!("web.{name}"))
            .collect();
        let mut changes = Vec::new();
        for probe_key in &non_green {
            graph.propagate_pending(probe_key, &mut state.services, &mut changes);
        }

        // Nothing should change — all probes are green, no propagation needed.
        assert!(changes.is_empty());
        assert!(state.services["web"].probes["ready"].state.is_green());
    }

    #[test]
    fn propagate_pending_from_red_probe_marks_dependents_pending() {
        // When a probe goes red, its green dependents should go pending.
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // web.http goes red — propagate pending
        state
            .services
            .get_mut("web")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::Stopped);
        let mut changes = Vec::new();
        graph.propagate_pending("web.http", &mut state.services, &mut changes);

        // web.ready (depends on web.http, was green) → now RED (source is Red)
        assert!(state.services["web"].probes["ready"].state.is_red());
        assert!(
            changes
                .iter()
                .any(|(pr, new, _)| pr.to_string() == "web.ready" && new.is_red())
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

        // Mark web.http as red (simulating it was red then its dep recovered)
        state
            .services
            .get_mut("web")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::DepRed {
            dep: ProbeRef::new("db", "ready"),
        });

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

        // web.ready should be pending (depends on web.http)
        assert!(state.services["web"].probes["ready"].state.is_pending());

        // app.ready should NOT be affected (doesn't depend on web.http)
        assert!(state.services["app"].probes["ready"].state.is_green());
        // app.http should NOT be affected
        assert!(state.services["app"].probes["http"].state.is_green());
    }

    #[test]
    fn all_pending_then_resolve_in_topo_order() {
        // Simulates reprobe-all: everything pending, probes all pass, resolve in topo order
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Set everything running + pending (simulates force reprobe-all)
        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
            }
        }

        // Simulate: all probes pass, resolve in topo order
        let all_svcs: Vec<String> = state.services.keys().cloned().collect();
        let levels = graph.topo_levels(&all_svcs);

        for level in &levels {
            for svc_name in level {
                let svc = &state.services[svc_name];
                let probe_names: Vec<String> = svc.probes.keys().cloned().collect();
                for probe_name in &probe_names {
                    let probe = &state.services[svc_name].probes[probe_name];
                    if probe.is_meta() {
                        continue; // Meta probes resolved by separate step
                    }
                    // "Probe passes" -- check deps to determine state
                    let deps_all_green = probe.depends_on.iter().all(|dep| {
                        state
                            .services
                            .get(&dep.service)
                            .and_then(|s| s.probes.get(&dep.probe))
                            .is_some_and(|p| p.state.is_green())
                    });
                    let new_state = if deps_all_green {
                        ProbeState::Green
                    } else {
                        ProbeState::Pending(crate::model::PendingReason::DepNotReady {
                            dep: probe
                                .depends_on
                                .first()
                                .cloned()
                                .unwrap_or_else(|| ProbeRef::new(svc_name, probe_name)),
                        })
                    };
                    state
                        .services
                        .get_mut(svc_name)
                        .unwrap()
                        .probes
                        .get_mut(probe_name)
                        .unwrap()
                        .state = new_state.clone();

                    // Recovery propagation if went green
                    if new_state.is_green() {
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
                    if !probe.is_meta() {
                        continue;
                    }
                    let satisfied = crate::probe::meta::is_satisfied(
                        &ProbeRef::new(svc_name, probe_name),
                        &state.services,
                    );
                    let new_state = if satisfied {
                        ProbeState::Green
                    } else {
                        ProbeState::Red(crate::model::RedReason::DepRed {
                            dep: ProbeRef::new(svc_name, probe_name),
                        })
                    };
                    state
                        .services
                        .get_mut(svc_name)
                        .unwrap()
                        .probes
                        .get_mut(probe_name)
                        .unwrap()
                        .state = new_state.clone();
                    if new_state.is_green() {
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
                assert!(
                    probe.state.is_green(),
                    "{svc_name}.{probe_name} should be green but is {:?}",
                    probe.state
                );
            }
        }
    }

    #[test]
    fn red_propagates_to_pending_dependents() {
        // If app.http is pending and db.ready goes red, app.http should go red (not stay pending)
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
        // Set app.http to pending, db.ready to green
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
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

        // app.http was pending, dep went red → should now be red
        assert!(
            state.services["app"].probes["http"].state.is_red(),
            "pending probe with red dep should become red"
        );
    }

    #[test]
    fn recovery_blocked_by_other_red_dep() {
        // app.http depends on db.ready AND redis.port.
        // db.ready recovers, but redis.port is still red.
        // app.http should stay red (not go pending).
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
                probe.state = ProbeState::Red(crate::model::RedReason::Stopped);
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
        assert!(
            state.services["app"].probes["http"].state.is_red(),
            "should stay red when another dep is still red"
        );
    }

    // ── initialize_probe_states tests ──

    #[test]
    fn init_probes_all_running_no_deps() {
        // db and redis have no start_after deps, both running → probes become Pending(Unchecked)
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        for svc in state.services.values_mut() {
            svc.state = ServiceState::Running;
        }
        graph.initialize_probe_states(&mut state.services);
        // All probes should be Pending(Unchecked)
        for (svc_name, svc) in &state.services {
            for (probe_name, probe) in &svc.probes {
                assert!(
                    probe.state.is_pending(),
                    "{svc_name}.{probe_name} should be pending but is {:?}",
                    probe.state
                );
            }
        }
    }

    #[test]
    fn init_probes_stopped_service_stays_red() {
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        // Leave all services stopped (default)
        graph.initialize_probe_states(&mut state.services);
        for (svc_name, svc) in &state.services {
            for (probe_name, probe) in &svc.probes {
                assert!(
                    probe.state.is_red(),
                    "{svc_name}.{probe_name} should be red but is {:?}",
                    probe.state
                );
            }
        }
    }

    #[test]
    fn init_probes_running_with_stopped_dep() {
        // web depends on db (start_after: [db.ready]).
        // db is stopped, web is running → web probes stay Red
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        // Only web is running, db is stopped
        state.services.get_mut("web").unwrap().state = ServiceState::Running;
        graph.initialize_probe_states(&mut state.services);
        for (probe_name, probe) in &state.services["web"].probes {
            assert!(
                probe.state.is_red(),
                "web.{probe_name} should be red (dep db is stopped) but is {:?}",
                probe.state
            );
        }
    }

    #[test]
    fn init_probes_partial_running() {
        // db running, redis stopped, web running (depends on db), app running (depends on db+redis)
        // web probes → Pending (db is running)
        // app probes → Red (redis is stopped, app start_after includes redis.ready)
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        state.services.get_mut("db").unwrap().state = ServiceState::Running;
        state.services.get_mut("web").unwrap().state = ServiceState::Running;
        state.services.get_mut("app").unwrap().state = ServiceState::Running;
        // redis stays Stopped
        graph.initialize_probe_states(&mut state.services);

        // db probes: pending (running, no deps)
        for probe in state.services["db"].probes.values() {
            assert!(probe.state.is_pending(), "db probe should be pending");
        }
        // web.http depends on db.ready which is pending (not red) → web.http should be pending
        // Actually wait — web's start_after deps are all running (db), so web probes can be pending.
        // But web.http depends_on db.ready, and db.ready is pending, not red.
        // Our logic only keeps probes Red if a dep is red, otherwise Pending(Unchecked).
        // Pending dep is fine — the probe will get reprobed anyway.
        for probe in state.services["web"].probes.values() {
            assert!(probe.state.is_pending(), "web probe should be pending");
        }
        // app: start_after includes redis.ready, redis is stopped → probes stay Red
        for (probe_name, probe) in &state.services["app"].probes {
            assert!(
                probe.state.is_red(),
                "app.{probe_name} should be red (redis stopped)"
            );
        }
    }

    #[test]
    fn init_probes_crashed_stays_red() {
        let config = demo_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);
        state.services.get_mut("db").unwrap().state = ServiceState::Crashed;
        graph.initialize_probe_states(&mut state.services);
        for probe in state.services["db"].probes.values() {
            assert!(probe.state.is_red(), "crashed service probe should be red");
        }
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
            .state = ProbeState::Red(crate::model::RedReason::Stopped);

        // full target should not be green (infra dep is red)
        let full_state = state.targets["full"].state(&state.services, &state.targets);
        assert!(
            !full_state.is_green(),
            "target should not be green when dependent target's probes are red"
        );
    }

    #[test]
    fn propagation_walks_through_stopped_service() {
        // db (stopped) → db.ready → app.http (running)
        // Stopping db should propagate red to app.http even though db is stopped.
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        // Both services running + green
        for svc in state.services.values_mut() {
            svc.state = ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // Stop db — mark its probes red
        state.services.get_mut("db").unwrap().state = ServiceState::Stopped;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::Stopped);

        // Propagate from db.port — should walk through db.ready (stopped) to reach app.http (running)
        let mut changes = Vec::new();
        graph.propagate_pending("db.port", &mut state.services, &mut changes);

        // app.http must be red — the propagation walked through stopped db.ready
        assert!(
            state.services["app"].probes["http"].state.is_red(),
            "app.http should be red: propagation must walk through stopped service probes"
        );
        // app.ready depends on app.http → also red
        assert!(
            state.services["app"].probes["ready"].state.is_red(),
            "app.ready should be red transitively"
        );
        // db.ready state should NOT be changed (db is stopped)
        assert!(
            state.services["db"].probes["ready"].state.is_green(),
            "stopped service probe state should not be modified"
        );
    }

    #[test]
    fn recovery_skips_stopped_services() {
        // db is stopped, db.port recovers. Recovery should NOT cross into
        // stopped db — db.ready is on a stopped service, skip it.
        // app.http stays Red(DepRed) because db.ready is still Red(Stopped).
        // The has_other_red guard handles this correctly.
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        for svc in state.services.values_mut() {
            svc.state = ServiceState::Running;
        }
        state.services.get_mut("db").unwrap().state = ServiceState::Stopped;

        for probe in state.services.get_mut("db").unwrap().probes.values_mut() {
            probe.state = ProbeState::Red(crate::model::RedReason::Stopped);
        }
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::DepRed {
            dep: ProbeRef::new("db", "ready"),
        });

        // db.port recovers
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

        // app.http stays Red — db.ready is still Red(Stopped), recovery skipped stopped service
        assert!(
            state.services["app"].probes["http"].state.is_red(),
            "app.http should stay red — db is still stopped"
        );
        assert!(
            changes.is_empty(),
            "no changes — recovery can't cross stopped service"
        );
    }

    #[test]
    fn log_since_not_advanced_on_propagation() {
        // log_since is only advanced on service restart (start.rs), not during
        // dep propagation. A running service's log output is still valid even
        // if a dependency goes red — the probe state (Red/DepRed) already
        // captures the dep failure. Reprobe can match existing logs.
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        for svc in state.services.values_mut() {
            svc.state = ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        let app_log_since_before = state.services["app"].log_since;

        // Stop db, mark db.port red, propagate
        state.services.get_mut("db").unwrap().state = ServiceState::Stopped;
        state
            .services
            .get_mut("db")
            .unwrap()
            .probes
            .get_mut("port")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::Stopped);
        let mut changes = Vec::new();
        graph.propagate_pending("db.port", &mut state.services, &mut changes);

        // app's log_since must NOT be advanced — only restart advances it
        assert_eq!(
            state.services["app"].log_since, app_log_since_before,
            "propagation should not advance log_since"
        );
    }

    #[test]
    fn recovery_does_not_overwrite_probe_failed() {
        // app.http depends on db.ready. Both are on running services.
        // app.http fails its probe check → Red(ProbeFailed).
        // db.ready recovers → propagate_recovery should NOT overwrite app.http
        // because it failed for its own reason, not because of db.
        let config = test_config();
        let graph = DependencyGraph::build(&config).unwrap();
        let mut state = crate::model::RuntimeState::from_config(&config);

        for svc in state.services.values_mut() {
            svc.state = crate::model::ServiceState::Running;
            for probe in svc.probes.values_mut() {
                probe.state = ProbeState::Green;
            }
        }

        // app.http fails its own probe check
        state
            .services
            .get_mut("app")
            .unwrap()
            .probes
            .get_mut("http")
            .unwrap()
            .state = ProbeState::Red(crate::model::RedReason::ProbeFailed(
            crate::model::ProbeFailure {
                error: "connection refused".into(),
                duration_ms: 100,
            },
        ));

        // db.ready recovers → propagate recovery
        let mut changes = Vec::new();
        graph.propagate_recovery("db.ready", &mut state.services, &mut changes);

        // app.http must STILL be Red(ProbeFailed) — recovery can't fix a probe's own failure
        assert!(
            matches!(
                state.services["app"].probes["http"].state,
                ProbeState::Red(crate::model::RedReason::ProbeFailed(_))
            ),
            "recovery propagation must not overwrite ProbeFailed: got {:?}",
            state.services["app"].probes["http"].state
        );
    }
}
