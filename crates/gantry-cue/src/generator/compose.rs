use indexmap::IndexMap;
use serde::Serialize;

use crate::error::Result;

use super::schema::{ImageDef, SetupJson, VolumeConfig};

fn is_false(v: &bool) -> bool {
    !v
}

#[derive(Debug, Serialize)]
struct ComposeFile {
    services: IndexMap<String, ComposeService>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<IndexMap<String, VolumeConfig>>,
}

#[derive(Debug, Serialize)]
struct ComposeService {
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<ComposeBuild>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoint: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ports: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cap_add: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cap_drop: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    privileged: bool,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    labels: IndexMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    init: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_grace_period: Option<String>,
}

#[derive(Debug, Serialize)]
struct ComposeBuild {
    context: String,
    dockerfile: String,
}

/// Generate docker-compose.yml content from a SetupJson.
pub fn generate_compose(setup: &SetupJson) -> Result<String> {
    let setup_name = &setup.name;
    let mut services = IndexMap::new();

    // Inject gantry supervisor service
    services.insert(
        "gantry".to_string(),
        ComposeService {
            image: Some("ghcr.io/chaoyi/gantry:latest".to_string()),
            build: None,
            command: None,
            entrypoint: None,
            container_name: Some(format!("{setup_name}-gantry")),
            user: None,
            working_dir: None,
            hostname: None,
            environment: None,
            ports: Some(vec!["9090:9090".to_string()]),
            volumes: Some(vec![
                "/var/run/docker.sock:/var/run/docker.sock".to_string(),
                "./gantry.yaml:/etc/gantry/config.yaml:ro".to_string(),
            ]),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            privileged: false,
            labels: IndexMap::new(),
            extra_hosts: Vec::new(),
            init: None,
            stop_grace_period: None,
        },
    );

    // Add user services
    for (svc_name, svc_def) in &setup.services {
        let (image, build) = match &svc_def.image {
            ImageDef::Prebuilt(img) => (Some(img.clone()), None),
            ImageDef::Build { build: _ } => (
                None,
                Some(ComposeBuild {
                    context: format!("./services/{svc_name}"),
                    dockerfile: "Dockerfile".to_string(),
                }),
            ),
        };

        let environment = if svc_def.env.is_empty() {
            None
        } else {
            Some(svc_def.env.clone())
        };

        services.insert(
            svc_name.clone(),
            ComposeService {
                image,
                build,
                command: svc_def.command.clone(),
                entrypoint: svc_def.entrypoint.clone(),
                container_name: Some(svc_def.container_name.clone()),
                user: svc_def.user.clone(),
                working_dir: svc_def.working_dir.clone(),
                hostname: svc_def.hostname.clone(),
                environment,
                ports: if svc_def.ports.is_empty() {
                    None
                } else {
                    Some(svc_def.ports.clone())
                },
                volumes: if svc_def.volumes.is_empty() {
                    None
                } else {
                    Some(svc_def.volumes.clone())
                },
                cap_add: svc_def.cap_add.clone(),
                cap_drop: svc_def.cap_drop.clone(),
                privileged: svc_def.privileged.unwrap_or(false),
                labels: svc_def.labels.clone(),
                extra_hosts: svc_def.extra_hosts.clone(),
                init: svc_def.init,
                stop_grace_period: svc_def.stop_grace_period.clone(),
            },
        );
    }

    let top_volumes = if setup.volumes.is_empty() {
        None
    } else {
        Some(setup.volumes.clone())
    };

    let compose = ComposeFile {
        services,
        volumes: top_volumes,
    };
    Ok(serde_yaml::to_string(&compose)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "name": "demo",
          "services": {
            "db": {
              "container_name": "demo-db",
              "image": "postgres:16",
              "env": {"POSTGRES_PASSWORD": "dev"},
              "ports": ["5432"],
              "capabilities": {
                "port": {"probe": {"type": "tcp", "port": 5432}}
              }
            },
            "app": {
              "container_name": "demo-app",
              "image": {"build": {"context": ".", "dockerfile": "Dockerfile"}},
              "env": {"DATABASE_URL": "postgres://db:5432/app"},
              "ports": ["8080"],
              "capabilities": {
                "http": {"probe": {"type": "tcp", "port": 8080}}
              }
            }
          }
        }"#
    }

    #[test]
    fn generates_compose_yaml() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        // Parse back to verify structure
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let services = parsed["services"].as_mapping().unwrap();

        // Gantry service present
        assert!(services.contains_key(&serde_yaml::Value::String("gantry".into())));

        // User services present
        assert!(services.contains_key(&serde_yaml::Value::String("db".into())));
        assert!(services.contains_key(&serde_yaml::Value::String("app".into())));
    }

    #[test]
    fn gantry_service_correct() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        assert!(yaml.contains("ghcr.io/chaoyi/gantry:latest"));
        assert!(!yaml.contains("command:"));
        assert!(yaml.contains("/var/run/docker.sock"));
        assert!(yaml.contains("gantry.yaml:/etc/gantry/config.yaml:ro"));
        assert!(yaml.contains("9090:9090"));
        assert!(yaml.contains("container_name: demo-gantry"));
    }

    #[test]
    fn prebuilt_image_has_no_build() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        // Parse and check db service
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let db = &parsed["services"]["db"];
        assert_eq!(db["image"].as_str(), Some("postgres:16"));
        assert!(db["build"].is_null());
    }

    #[test]
    fn build_image_has_no_image_field() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let app = &parsed["services"]["app"];
        assert!(app["image"].is_null());
        assert_eq!(app["build"]["context"].as_str(), Some("./services/app"));
        assert_eq!(app["build"]["dockerfile"].as_str(), Some("Dockerfile"));
    }

    #[test]
    fn container_names() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        assert!(yaml.contains("container_name: demo-db"));
        assert!(yaml.contains("container_name: demo-app"));
        assert!(yaml.contains("container_name: demo-gantry"));
    }

    #[test]
    fn no_depends_on_or_healthcheck() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        assert!(!yaml.contains("depends_on"));
        assert!(!yaml.contains("healthcheck"));
    }

    #[test]
    fn environment_no_host_ports() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        assert!(yaml.contains("POSTGRES_PASSWORD"));
        assert!(yaml.contains("DATABASE_URL"));
        // User services get no host port mapping (Docker network handles it)
        assert!(!yaml.contains("5432:5432"));
        assert!(!yaml.contains("8080:8080"));
        // Only gantry gets host-mapped ports for the UI
        assert!(yaml.contains("9090:9090"));
    }

    #[test]
    fn new_compose_fields_pass_through() {
        let json = r#"{
          "name": "test",
          "services": {
            "vpn": {
              "container_name": "test-vpn",
              "image": "wireguard:latest",
              "entrypoint": ["/init.sh", "--start"],
              "user": "1000:1000",
              "working_dir": "/app",
              "hostname": "vpn-node",
              "cap_add": ["NET_ADMIN"],
              "cap_drop": ["ALL"],
              "privileged": true,
              "labels": {"com.example.env": "dev"},
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
        let setup: crate::generator::schema::SetupJson = serde_json::from_str(json).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        // Verify new fields appear in output
        assert!(yaml.contains("entrypoint:"), "missing entrypoint");
        assert!(
            yaml.contains("user:") && yaml.contains("1000:1000"),
            "missing user, yaml:\n{yaml}"
        );
        assert!(yaml.contains("working_dir: /app"), "missing working_dir");
        assert!(yaml.contains("hostname: vpn-node"), "missing hostname");
        assert!(yaml.contains("cap_add:"), "missing cap_add");
        assert!(yaml.contains("NET_ADMIN"), "missing NET_ADMIN in cap_add");
        assert!(yaml.contains("cap_drop:"), "missing cap_drop");
        assert!(yaml.contains("privileged: true"), "missing privileged");
        assert!(yaml.contains("com.example.env"), "missing labels");
        assert!(yaml.contains("extra_hosts:"), "missing extra_hosts");
        assert!(
            yaml.contains("host.docker.internal:host-gateway"),
            "missing extra_hosts entry"
        );
        assert!(yaml.contains("init: true"), "missing init");
        assert!(
            yaml.contains("stop_grace_period:"),
            "missing stop_grace_period"
        );

        // Top-level volumes
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let vols = parsed["volumes"].as_mapping().unwrap();
        assert_eq!(vols.len(), 2);
    }

    #[test]
    fn new_fields_omitted_when_empty() {
        // Existing sample_json has none of the new fields — they should not appear
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup).unwrap();

        assert!(
            !yaml.contains("entrypoint:"),
            "entrypoint should be omitted"
        );
        assert!(!yaml.contains("user:"), "user should be omitted");
        assert!(
            !yaml.contains("working_dir:"),
            "working_dir should be omitted"
        );
        assert!(!yaml.contains("hostname:"), "hostname should be omitted");
        assert!(!yaml.contains("cap_add:"), "cap_add should be omitted");
        assert!(!yaml.contains("cap_drop:"), "cap_drop should be omitted");
        assert!(
            !yaml.contains("privileged:"),
            "privileged should be omitted"
        );
        assert!(!yaml.contains("labels:"), "labels should be omitted");
        assert!(
            !yaml.contains("extra_hosts:"),
            "extra_hosts should be omitted"
        );
        assert!(!yaml.contains("init:"), "init should be omitted");
        assert!(
            !yaml.contains("stop_grace_period:"),
            "stop_grace_period should be omitted"
        );
        // No top-level volumes key
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(
            parsed["volumes"].is_null(),
            "top-level volumes should be omitted"
        );
    }
}
