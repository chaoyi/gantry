use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

/// Top-level JSON from `cue export`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetupJson {
    pub services: IndexMap<String, ServiceDef>,
    #[serde(default)]
    pub targets: IndexMap<String, TargetDef>,
    #[serde(default)]
    pub defaults: Option<DefaultsDef>,
    #[serde(default)]
    pub files: Vec<FileDef>,
}

/// A service definition from the CUE export.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ServiceDef {
    pub image: ImageDef,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub command: Option<serde_json::Value>,
    #[serde(default)]
    pub files: Vec<FileDef>,
    #[serde(default)]
    pub start_after: Vec<String>,
    #[serde(default)]
    pub restart_on_fail: Option<bool>,
    #[serde(default)]
    pub probes: IndexMap<String, ProbeEntryDef>,
}

/// Image: pre-built string or build definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ImageDef {
    Prebuilt(String),
    Build { build: BuildDef },
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildDef {
    pub context: String,
    pub dockerfile: String,
}

/// A file to copy or render as a template.
#[derive(Debug, Clone, Deserialize)]
pub struct FileDef {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub template: bool,
    #[serde(default)]
    pub context: Option<Value>,
}

/// Probe entry from CUE export.
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeEntryDef {
    pub probe: ProbeDef,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Probe definition — pass-through to gantry.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeDef {
    #[serde(rename = "type")]
    pub probe_type: String,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetDef {
    pub probes: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsDef {
    #[serde(default)]
    pub tcp_probe_timeout: Option<String>,
    #[serde(default)]
    pub log_probe_timeout: Option<String>,
    #[serde(default)]
    pub restart_on_fail: Option<bool>,
    #[serde(default)]
    pub probe_backoff: Option<BackoffDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackoffDef {
    #[serde(default)]
    pub initial: Option<String>,
    #[serde(default)]
    pub max: Option<String>,
    #[serde(default)]
    pub multiplier: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json() -> &'static str {
        r#"{
          "services": {
            "app": {
              "image": {"build": {"context": ".", "dockerfile": "Dockerfile"}},
              "env": {"PORT": "8080", "DATABASE_URL": "postgres://db:5432/app"},
              "ports": ["8080"],
              "files": [
                {
                  "src": "services/web-app/config.toml.tmpl",
                  "dst": "config.toml",
                  "template": true,
                  "context": {"port": 8080, "log_level": "info", "redis_url": "redis://cache:6379"}
                },
                {"src": "services/web-app/migrations/", "dst": "migrations/"}
              ],
              "start_after": ["db.ready", "cache.ready"],
              "probes": {
                "http":  {"probe": {"type": "tcp", "port": 8080}, "depends_on": ["db.ready"]},
                "ready": {"probe": {"type": "meta"}, "depends_on": ["app.http"]}
              }
            }
          },
          "targets": {
            "integration": {
              "probes": ["app.ready", "worker.ready"],
              "depends_on": ["db-ready"]
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
    fn parse_spec_json() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        assert_eq!(setup.services.len(), 1);
        assert!(setup.services.contains_key("app"));
        assert_eq!(setup.targets.len(), 1);
        assert!(setup.defaults.is_some());
    }

    #[test]
    fn parse_image_build() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        let app = &setup.services["app"];
        match &app.image {
            ImageDef::Build { build } => {
                assert_eq!(build.context, ".");
                assert_eq!(build.dockerfile, "Dockerfile");
            }
            ImageDef::Prebuilt(_) => panic!("expected Build variant"),
        }
    }

    #[test]
    fn parse_image_prebuilt() {
        let json = r#"{
          "services": {
            "db": {
              "image": "postgres:16",
              "probes": {
                "port": {"probe": {"type": "tcp", "port": 5432}}
              }
            }
          }
        }"#;
        let setup: SetupJson = serde_json::from_str(json).unwrap();
        match &setup.services["db"].image {
            ImageDef::Prebuilt(img) => assert_eq!(img, "postgres:16"),
            ImageDef::Build { .. } => panic!("expected Prebuilt variant"),
        }
    }

    #[test]
    fn parse_files() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        let files = &setup.services["app"].files;
        assert_eq!(files.len(), 2);

        assert!(files[0].template);
        assert_eq!(files[0].dst, "config.toml");
        assert!(files[0].context.is_some());

        assert!(!files[1].template);
        assert_eq!(files[1].src, "services/web-app/migrations/");
    }

    #[test]
    fn parse_probes() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        let probes = &setup.services["app"].probes;
        assert_eq!(probes.len(), 2);

        let http = &probes["http"];
        assert_eq!(http.probe.probe_type, "tcp");
        assert_eq!(http.probe.extra["port"], 8080);
        assert_eq!(http.depends_on, vec!["db.ready"]);

        let ready = &probes["ready"];
        assert_eq!(ready.probe.probe_type, "meta");
        assert_eq!(ready.depends_on, vec!["app.http"]);
    }

    #[test]
    fn parse_targets() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        let tgt = &setup.targets["integration"];
        assert_eq!(tgt.probes, vec!["app.ready", "worker.ready"]);
        assert_eq!(tgt.depends_on, vec!["db-ready"]);
    }

    #[test]
    fn parse_defaults() {
        let setup: SetupJson = serde_json::from_str(spec_json()).unwrap();
        let defaults = setup.defaults.unwrap();
        assert_eq!(defaults.tcp_probe_timeout.as_deref(), Some("10s"));
        assert_eq!(defaults.log_probe_timeout.as_deref(), Some("30s"));
        let backoff = defaults.probe_backoff.unwrap();
        assert_eq!(backoff.initial.as_deref(), Some("100ms"));
        assert_eq!(backoff.max.as_deref(), Some("5s"));
        assert_eq!(backoff.multiplier, Some(2.0));
    }

    #[test]
    fn parse_minimal() {
        let json = r#"{"services": {}}"#;
        let setup: SetupJson = serde_json::from_str(json).unwrap();
        assert!(setup.services.is_empty());
        assert!(setup.targets.is_empty());
        assert!(setup.defaults.is_none());
    }
}
