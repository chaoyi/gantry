use indexmap::IndexMap;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

use crate::error::{GantryError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct GantryConfig {
    pub services: IndexMap<String, ServiceConfig>,
    pub targets: IndexMap<String, TargetConfig>,
    #[serde(default)]
    pub defaults: DefaultsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub container: String,
    #[serde(default)]
    pub start_after: Vec<String>,
    #[serde(default)]
    pub probes: IndexMap<String, ProbeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeEntry {
    pub probe: ProbeConfig,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ProbeConfig {
    #[serde(rename = "tcp")]
    Tcp {
        /// Host to probe. Defaults to the service name (docker-compose DNS).
        #[serde(default)]
        host: Option<String>,
        port: u16,
        #[serde(
            default = "default_tcp_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
    },
    #[serde(rename = "log")]
    Log {
        success: String,
        #[serde(default)]
        failure: Option<String>,
        #[serde(
            default = "default_log_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
    },
    #[serde(rename = "meta")]
    Meta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetConfig {
    pub probes: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    #[serde(
        default = "default_tcp_timeout",
        deserialize_with = "deserialize_duration"
    )]
    pub tcp_probe_timeout: Duration,
    #[serde(
        default = "default_log_timeout",
        deserialize_with = "deserialize_duration"
    )]
    pub log_probe_timeout: Duration,
    #[serde(default)]
    pub probe_backoff: BackoffConfig,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            tcp_probe_timeout: default_tcp_timeout(),
            log_probe_timeout: default_log_timeout(),
            probe_backoff: BackoffConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackoffConfig {
    #[serde(
        default = "default_backoff_initial",
        deserialize_with = "deserialize_duration"
    )]
    pub initial: Duration,
    #[serde(
        default = "default_backoff_max",
        deserialize_with = "deserialize_duration"
    )]
    pub max: Duration,
    #[serde(default = "default_backoff_multiplier")]
    pub multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: default_backoff_initial(),
            max: default_backoff_max(),
            multiplier: default_backoff_multiplier(),
        }
    }
}

fn default_tcp_timeout() -> Duration {
    Duration::from_secs(10)
}
fn default_log_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_backoff_initial() -> Duration {
    Duration::from_millis(100)
}
fn default_backoff_max() -> Duration {
    Duration::from_secs(5)
}
fn default_backoff_multiplier() -> f64 {
    2.0
}

fn deserialize_duration<'de, D>(deserializer: D) -> std::result::Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

pub fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        let n: u64 = ms
            .trim()
            .parse()
            .map_err(|e| format!("invalid duration: {e}"))?;
        Ok(Duration::from_millis(n))
    } else if let Some(secs) = s.strip_suffix('s') {
        let n: u64 = secs
            .trim()
            .parse()
            .map_err(|e| format!("invalid duration: {e}"))?;
        Ok(Duration::from_secs(n))
    } else if let Some(mins) = s.strip_suffix('m') {
        let n: u64 = mins
            .trim()
            .parse()
            .map_err(|e| format!("invalid duration: {e}"))?;
        Ok(Duration::from_secs(n * 60))
    } else {
        Err(format!(
            "invalid duration format: {s} (expected e.g. 100ms, 10s, 2m)"
        ))
    }
}

impl GantryConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| GantryError::Config(format!("failed to read {}: {e}", path.display())))?;
        let mut config: GantryConfig = serde_yaml::from_str(&content)?;
        config.auto_generate_ready_probes();
        config.topo_sort_probes();
        Ok(config)
    }

    /// Topologically sort probes within each service so sources come before dependents.
    /// Falls back to original order for probes in cycles.
    pub(crate) fn topo_sort_probes(&mut self) {
        use petgraph::algo::toposort;
        use petgraph::graph::DiGraph;
        use std::collections::HashMap;

        for (svc_name, svc) in &mut self.services {
            let probe_names: Vec<String> = svc.probes.keys().cloned().collect();
            if probe_names.len() <= 1 {
                continue;
            }

            let mut graph = DiGraph::<String, ()>::new();
            let mut node_map: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();

            for pn in &probe_names {
                let idx = graph.add_node(pn.clone());
                node_map.insert(pn.clone(), idx);
            }

            for (pn, probe) in svc.probes.iter() {
                let to = node_map[pn];
                for dep in &probe.depends_on {
                    // Only intra-service deps affect ordering
                    if let Some((dep_svc, dep_probe)) = dep.split_once('.')
                        && dep_svc == svc_name
                        && let Some(&from) = node_map.get(dep_probe)
                    {
                        graph.add_edge(from, to, ());
                    }
                }
            }

            let sorted = match toposort(&graph, None) {
                Ok(order) => order
                    .into_iter()
                    .map(|idx| graph[idx].clone())
                    .collect::<Vec<_>>(),
                Err(_) => continue, // Cycle — keep original order
            };

            let mut new_probes = IndexMap::new();
            for pn in &sorted {
                if let Some(probe) = svc.probes.swap_remove(pn) {
                    new_probes.insert(pn.clone(), probe);
                }
            }
            svc.probes = new_probes;
        }
    }

    pub(crate) fn auto_generate_ready_probes(&mut self) {
        for (svc_name, svc) in &mut self.services {
            if svc.probes.contains_key("ready") {
                continue;
            }
            let dep_probes: Vec<String> = svc
                .probes
                .keys()
                .map(|probe_name| format!("{svc_name}.{probe_name}"))
                .collect();
            if dep_probes.is_empty() {
                continue;
            }
            svc.probes.insert(
                "ready".to_string(),
                ProbeEntry {
                    probe: ProbeConfig::Meta,
                    depends_on: dep_probes,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn parse_minimal_config() {
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
targets:
  db-ready:
    probes: [db.ready]
"#;
        let config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.services.contains_key("db"));
        assert!(config.targets.contains_key("db-ready"));
    }

    #[test]
    fn auto_generate_ready() {
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
      accepting:
        probe:
          type: log
          success: "ready to accept connections"
          timeout: 30s
        depends_on: [db.port]
targets:
  db-ready:
    probes: [db.ready]
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.auto_generate_ready_probes();
        let db = &config.services["db"];
        assert!(db.probes.contains_key("ready"));
        let ready = &db.probes["ready"];
        assert!(matches!(ready.probe, ProbeConfig::Meta));
        assert!(ready.depends_on.contains(&"db.port".to_string()));
        assert!(ready.depends_on.contains(&"db.accepting".to_string()));
    }

    #[test]
    fn topo_sort_probes_order() {
        let yaml = r#"
services:
  app:
    container: app-1
    probes:
      ready:
        probe: { type: meta }
        depends_on: [app.http]
      http:
        probe: { type: tcp, port: 8080, timeout: 10s }
        depends_on: [app.init]
      init:
        probe: { type: log, success: "ready", timeout: 10s }
targets: {}
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.topo_sort_probes();
        let probe_names: Vec<&str> = config.services["app"]
            .probes
            .keys()
            .map(|s| s.as_str())
            .collect();
        // init before http before ready
        assert_eq!(probe_names, vec!["init", "http", "ready"]);
    }

    #[test]
    fn topo_sort_probes_cycle_keeps_original() {
        let yaml = r#"
services:
  svc:
    container: svc-1
    probes:
      a:
        probe: { type: tcp, port: 1, timeout: 1s }
        depends_on: [svc.b]
      b:
        probe: { type: tcp, port: 2, timeout: 1s }
        depends_on: [svc.a]
targets: {}
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.topo_sort_probes();
        // Cycle → original order preserved
        let probe_names: Vec<&str> = config.services["svc"]
            .probes
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(probe_names, vec!["a", "b"]);
    }

    #[test]
    fn topo_sort_ignores_cross_service_deps() {
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
        depends_on: [db.port]
      init:
        probe: { type: log, success: "ok", timeout: 10s }
targets: {}
"#;
        let mut config: GantryConfig = serde_yaml::from_str(yaml).unwrap();
        config.topo_sort_probes();
        // http depends on db.port (cross-service) — doesn't affect intra-service ordering
        // init and http have no intra-service dep, so order is preserved
        let probe_names: Vec<&str> = config.services["app"]
            .probes
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert!(probe_names.contains(&"http"));
        assert!(probe_names.contains(&"init"));
    }
}
