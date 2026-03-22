use indexmap::IndexMap;
use serde::Serialize;

use crate::error::Result;

use super::schema::{ImageDef, SetupJson};

#[derive(Debug, Serialize)]
struct ComposeFile {
    services: IndexMap<String, ComposeService>,
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
    container_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ports: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct ComposeBuild {
    context: String,
    dockerfile: String,
}

/// Generate docker-compose.yml content from a SetupJson.
pub fn generate_compose(setup: &SetupJson, setup_name: &str) -> Result<String> {
    let mut services = IndexMap::new();

    // Inject gantry supervisor service
    services.insert(
        "gantry".to_string(),
        ComposeService {
            image: Some("ghcr.io/chaoyi/gantry:latest".to_string()),
            build: None,
            command: None,
            container_name: Some(format!("{setup_name}-gantry")),
            environment: None,
            ports: Some(vec!["9090:9090".to_string()]),
            volumes: Some(vec![
                "/var/run/docker.sock:/var/run/docker.sock".to_string(),
                "./gantry.yaml:/etc/gantry/config.yaml:ro".to_string(),
            ]),
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
                container_name: Some(format!("{setup_name}-{svc_name}")),
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
            },
        );
    }

    let compose = ComposeFile { services };
    Ok(serde_yaml::to_string(&compose)?)
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
              "capabilities": {
                "port": {"probe": {"type": "tcp", "port": 5432}}
              }
            },
            "app": {
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
        let yaml = generate_compose(&setup, "demo").unwrap();

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
        let yaml = generate_compose(&setup, "demo").unwrap();

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
        let yaml = generate_compose(&setup, "demo").unwrap();

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
        let yaml = generate_compose(&setup, "demo").unwrap();

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
        let yaml = generate_compose(&setup, "mysetup").unwrap();

        assert!(yaml.contains("container_name: mysetup-db"));
        assert!(yaml.contains("container_name: mysetup-app"));
        assert!(yaml.contains("container_name: mysetup-gantry"));
    }

    #[test]
    fn no_depends_on_or_healthcheck() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup, "demo").unwrap();

        assert!(!yaml.contains("depends_on"));
        assert!(!yaml.contains("healthcheck"));
    }

    #[test]
    fn environment_no_host_ports() {
        let setup: crate::generator::schema::SetupJson =
            serde_json::from_str(sample_json()).unwrap();
        let yaml = generate_compose(&setup, "demo").unwrap();

        assert!(yaml.contains("POSTGRES_PASSWORD"));
        assert!(yaml.contains("DATABASE_URL"));
        // User services get no host port mapping (Docker network handles it)
        assert!(!yaml.contains("5432:5432"));
        assert!(!yaml.contains("8080:8080"));
        // Only gantry gets host-mapped ports for the UI
        assert!(yaml.contains("9090:9090"));
    }
}
