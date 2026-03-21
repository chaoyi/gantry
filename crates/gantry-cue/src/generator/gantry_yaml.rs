use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use crate::error::Result;

use super::schema::{DefaultsDef, SetupJson};

/// Serialization structs for gantry.yaml output.
/// Field names match what the gantry supervisor expects.
#[derive(Debug, Serialize)]
struct GantryYaml {
    services: IndexMap<String, GantryService>,
    targets: IndexMap<String, GantryTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defaults: Option<GantryDefaults>,
}

#[derive(Debug, Serialize)]
struct GantryService {
    container: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    start_after: Vec<String>,
    probes: IndexMap<String, GantryProbe>,
}

#[derive(Debug, Serialize)]
struct GantryProbe {
    probe: IndexMap<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GantryTarget {
    probes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GantryDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    tcp_probe_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_probe_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_backoff: Option<GantryBackoff>,
}

#[derive(Debug, Serialize)]
struct GantryBackoff {
    #[serde(skip_serializing_if = "Option::is_none")]
    initial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multiplier: Option<f64>,
}

/// Generate gantry.yaml content from a SetupJson.
///
/// Maps CUE's "probes" to gantry's "probes" field name.
pub fn generate_gantry_yaml(setup: &SetupJson, setup_name: &str) -> Result<String> {
    let mut services = IndexMap::new();

    for (svc_name, svc_def) in &setup.services {
        let mut probes = IndexMap::new();
        for (probe_name, entry) in &svc_def.probes {
            let mut probe = IndexMap::new();
            probe.insert(
                "type".to_string(),
                Value::String(entry.probe.probe_type.clone()),
            );
            for (k, v) in &entry.probe.extra {
                probe.insert(k.clone(), v.clone());
            }
            probes.insert(
                probe_name.clone(),
                GantryProbe {
                    probe,
                    depends_on: entry.depends_on.clone(),
                },
            );
        }

        services.insert(
            svc_name.clone(),
            GantryService {
                container: format!("{setup_name}-{svc_name}"),
                start_after: svc_def.start_after.clone(),
                probes,
            },
        );
    }

    let mut targets = IndexMap::new();
    for (tgt_name, tgt_def) in &setup.targets {
        targets.insert(
            tgt_name.clone(),
            GantryTarget {
                probes: tgt_def.probes.clone(),
                depends_on: tgt_def.depends_on.clone(),
            },
        );
    }

    let defaults = setup.defaults.as_ref().map(convert_defaults);

    let yaml = GantryYaml {
        services,
        targets,
        defaults,
    };

    Ok(serde_yaml::to_string(&yaml)?)
}

fn convert_defaults(d: &DefaultsDef) -> GantryDefaults {
    GantryDefaults {
        tcp_probe_timeout: d.tcp_probe_timeout.clone(),
        log_probe_timeout: d.log_probe_timeout.clone(),
        probe_backoff: d.probe_backoff.as_ref().map(|b| GantryBackoff {
            initial: b.initial.clone(),
            max: b.max.clone(),
            multiplier: b.multiplier,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "services": {
            "db": {
              "image": "postgres:16",
              "env": {"POSTGRES_PASSWORD": "dev"},
              "ports": ["5432"],
              "probes": {
                "port": {"probe": {"type": "tcp", "port": 5432}, "depends_on": []},
                "accepting": {
                  "probe": {"type": "log", "success": "ready to accept connections"},
                  "depends_on": ["db.port"]
                }
              }
            },
            "app": {
              "image": {"build": {"context": ".", "dockerfile": "Dockerfile"}},
              "env": {"DATABASE_URL": "postgres://db:5432/app"},
              "ports": ["8080"],
              "start_after": ["db.ready"],
              "probes": {
                "http": {"probe": {"type": "tcp", "port": 8080}, "depends_on": ["db.ready"]}
              }
            }
          },
          "targets": {
            "integration": {
              "probes": ["app.ready"],
              "depends_on": []
            }
          },
          "defaults": {
            "tcp_probe_timeout": "10s",
            "log_probe_timeout": "30s",
            "probe_backoff": {"initial": "100ms", "max": "5s", "multiplier": 2}
          }
        }"#
    }

    #[test]
    fn generates_valid_yaml() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_gantry_yaml(&setup, "demo").unwrap();

        assert!(yaml.contains("container: demo-db"));
        assert!(yaml.contains("container: demo-app"));
        assert!(yaml.contains("type: tcp"));
        assert!(yaml.contains("type: log"));
        // Output uses "probes" (gantry's field name), not "probes"
        assert!(yaml.contains("probes:"));
        assert!(!yaml.contains("capabilities:"));
    }

    #[test]
    fn round_trip_through_gantry_config() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_gantry_yaml(&setup, "demo").unwrap();

        // The supervisor's GantryConfig must be able to parse our output
        let config: gantry::config::GantryConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services["db"].container, "demo-db");
        assert_eq!(config.services["app"].container, "demo-app");
        assert_eq!(config.services["app"].start_after, vec!["db.ready"]);

        // Probes preserved
        let http = &config.services["app"].probes["http"];
        assert_eq!(http.depends_on, vec!["db.ready"]);

        // Targets preserved
        assert_eq!(config.targets["integration"].probes, vec!["app.ready"]);

        // Defaults preserved
        assert_eq!(
            config.defaults.tcp_probe_timeout,
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn preserves_structure() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_gantry_yaml(&setup, "demo").unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        let services = parsed["services"].as_mapping().unwrap();
        assert_eq!(services.len(), 2);

        let app = &parsed["services"]["app"];
        assert_eq!(app["container"].as_str(), Some("demo-app"));

        let start_after = app["start_after"].as_sequence().unwrap();
        assert_eq!(start_after[0].as_str(), Some("db.ready"));

        let tgt = &parsed["targets"]["integration"];
        let probes = tgt["probes"].as_sequence().unwrap();
        assert_eq!(probes[0].as_str(), Some("app.ready"));

        assert_eq!(
            parsed["defaults"]["tcp_probe_timeout"].as_str(),
            Some("10s")
        );
    }

    #[test]
    fn no_defaults_when_absent() {
        let json = r#"{
          "services": {
            "db": {
              "image": "postgres:16",
              "probes": {
                "port": {"probe": {"type": "tcp", "port": 5432}}
              }
            }
          },
          "targets": {}
        }"#;
        let setup: crate::generator::schema::SetupJson = serde_json::from_str(json).unwrap();
        let yaml = generate_gantry_yaml(&setup, "test").unwrap();
        assert!(!yaml.contains("defaults"));
    }
}
