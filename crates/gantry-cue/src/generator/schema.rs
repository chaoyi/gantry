use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    pub volumes: IndexMap<String, VolumeConfig>,
}

/// Named volume configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
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
    pub entrypoint: Option<serde_json::Value>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub privileged: Option<bool>,
    #[serde(default)]
    pub labels: IndexMap<String, String>,
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    #[serde(default)]
    pub init: Option<bool>,
    #[serde(default)]
    pub stop_grace_period: Option<String>,
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
        assert!(setup.volumes.is_empty());
    }

    #[test]
    fn parse_new_compose_fields() {
        let json = r#"{
          "services": {
            "vpn": {
              "image": "wireguard:latest",
              "entrypoint": "/init.sh",
              "user": "1000:1000",
              "working_dir": "/app",
              "hostname": "vpn-node",
              "cap_add": ["NET_ADMIN", "SYS_MODULE"],
              "cap_drop": ["ALL"],
              "privileged": true,
              "labels": {"com.example.env": "dev", "com.example.team": "infra"},
              "extra_hosts": ["host.docker.internal:host-gateway"],
              "init": true,
              "stop_grace_period": "30s",
              "probes": {}
            }
          },
          "volumes": {
            "pgdata": {"driver": "local"},
            "cache": {}
          }
        }"#;
        let setup: SetupJson = serde_json::from_str(json).unwrap();
        let svc = &setup.services["vpn"];

        // entrypoint as string
        assert_eq!(svc.entrypoint.as_ref().unwrap().as_str(), Some("/init.sh"));
        assert_eq!(svc.user.as_deref(), Some("1000:1000"));
        assert_eq!(svc.working_dir.as_deref(), Some("/app"));
        assert_eq!(svc.hostname.as_deref(), Some("vpn-node"));
        assert_eq!(svc.cap_add, vec!["NET_ADMIN", "SYS_MODULE"]);
        assert_eq!(svc.cap_drop, vec!["ALL"]);
        assert_eq!(svc.privileged, Some(true));
        assert_eq!(svc.labels["com.example.env"], "dev");
        assert_eq!(svc.labels["com.example.team"], "infra");
        assert_eq!(svc.extra_hosts, vec!["host.docker.internal:host-gateway"]);
        assert_eq!(svc.init, Some(true));
        assert_eq!(svc.stop_grace_period.as_deref(), Some("30s"));

        // Top-level volumes
        assert_eq!(setup.volumes.len(), 2);
        assert_eq!(setup.volumes["pgdata"].driver.as_deref(), Some("local"));
        assert!(setup.volumes["cache"].driver.is_none());
    }

    #[test]
    fn parse_entrypoint_array() {
        let json = r#"{
          "services": {
            "app": {
              "image": "myapp:latest",
              "entrypoint": ["/bin/sh", "-c", "echo hello"],
              "probes": {}
            }
          }
        }"#;
        let setup: SetupJson = serde_json::from_str(json).unwrap();
        let ep = setup.services["app"].entrypoint.as_ref().unwrap();
        assert!(ep.is_array());
        assert_eq!(ep.as_array().unwrap().len(), 3);
    }
}
