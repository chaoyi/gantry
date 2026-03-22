pub mod compose;
pub mod files;
pub mod gantry_yaml;
pub mod schema;
pub mod validate;

use std::path::Path;

use crate::error::{GantryError, Result};

use schema::{ImageDef, SetupJson};

/// Generate output files from a setup directory containing CUE definitions.
///
/// Pipeline: cue export → JSON → validate → generate files.
pub fn generate(setup_dir: &Path) -> Result<()> {
    let setup_name = setup_dir
        .file_name()
        .ok_or_else(|| GantryError::Config("invalid setup directory".into()))?
        .to_str()
        .ok_or_else(|| GantryError::Config("non-UTF8 setup directory name".into()))?
        .to_string();

    let json_str = run_cue_export(setup_dir)?;

    // Use the project root (parent of the setup dir's parent) as base for file resolution.
    // For a setup at setups/demo/, the project root is the parent of setups/.
    // But file src paths in the JSON are relative to the project root (cwd), so we use cwd.
    let base_dir = std::env::current_dir()
        .map_err(|e| GantryError::Operation(format!("cannot determine cwd: {e}")))?;

    generate_from_json(&json_str, &setup_name, &base_dir)
}

/// Generate output from pre-exported JSON. Testable without CUE.
pub fn generate_from_json(json_str: &str, setup_name: &str, base_dir: &Path) -> Result<()> {
    let setup: SetupJson = serde_json::from_str(json_str)
        .map_err(|e| GantryError::Config(format!("failed to parse CUE export: {e}")))?;

    validate::validate(&setup, base_dir)?;

    let output_dir = base_dir.join("output").join(setup_name);
    write_output(&setup, setup_name, base_dir, &output_dir)
}

fn write_output(
    setup: &SetupJson,
    setup_name: &str,
    base_dir: &Path,
    output_dir: &Path,
) -> Result<()> {
    // Clean and create output directory
    if output_dir.exists() {
        std::fs::remove_dir_all(output_dir)?;
    }
    std::fs::create_dir_all(output_dir)?;

    // Generate docker-compose.yml
    let compose_yaml = compose::generate_compose(setup, setup_name)?;
    std::fs::write(output_dir.join("docker-compose.yml"), compose_yaml)?;

    // Generate gantry.yaml
    let gantry_yaml = gantry_yaml::generate_gantry_yaml(setup, setup_name)?;
    std::fs::write(output_dir.join("gantry.yaml"), gantry_yaml)?;

    // Process each service's files
    for (svc_name, svc_def) in &setup.services {
        let svc_output_dir = output_dir.join("services").join(svc_name);
        std::fs::create_dir_all(&svc_output_dir)?;

        if let ImageDef::Build { ref build } = svc_def.image {
            // Skip template source files — they're rendered by process_files
            let skip_srcs: Vec<&str> = svc_def
                .files
                .iter()
                .filter(|f| f.template)
                .map(|f| f.src.as_str())
                .collect();
            files::copy_build_context(build, base_dir, &svc_output_dir, &skip_srcs)?;
        }

        files::process_files(&svc_def.files, base_dir, &svc_output_dir)?;
    }

    // Process shared files (not belonging to any service)
    if !setup.files.is_empty() {
        let shared_dir = output_dir.join("shared");
        std::fs::create_dir_all(&shared_dir)?;
        files::process_files(&setup.files, base_dir, &shared_dir)?;
    }

    eprintln!("generated: {}", output_dir.display());
    Ok(())
}

fn run_cue_export(setup_dir: &Path) -> Result<String> {
    let output = std::process::Command::new("cue")
        .args(["export", "."])
        .current_dir(setup_dir)
        .output()
        .map_err(|e| GantryError::Config(format!("failed to run cue export: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GantryError::Config(format!("cue export failed:\n{stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gantry_test_gen_{name}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

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
              "image": {"build": {"context": "app-ctx", "dockerfile": "Dockerfile"}},
              "env": {"DATABASE_URL": "postgres://db:5432/app"},
              "ports": ["8080"],
              "files": [
                {
                  "src": "config.toml.tmpl",
                  "dst": "config.toml",
                  "template": true,
                  "context": {"port": 8080, "log_level": "info"}
                }
              ],
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
    fn end_to_end_generate() {
        let tmp = make_tmp("e2e");

        // Create source files the generator expects
        fs::create_dir_all(tmp.join("app-ctx")).unwrap();
        fs::write(tmp.join("app-ctx/Dockerfile"), "FROM alpine").unwrap();
        fs::write(
            tmp.join("config.toml.tmpl"),
            "[server]\nport = {{ port }}\nlog_level = \"{{ log_level }}\"\n",
        )
        .unwrap();

        generate_from_json(sample_json(), "demo", &tmp).unwrap();

        let out = tmp.join("output/demo");

        // docker-compose.yml exists and has expected content
        let compose = fs::read_to_string(out.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("gantry"));
        assert!(compose.contains("postgres:16"));
        assert!(compose.contains("./services/app"));

        // gantry.yaml exists and has expected structure
        let gantry_yaml = fs::read_to_string(out.join("gantry.yaml")).unwrap();
        assert!(gantry_yaml.contains("container: demo-db"));
        assert!(gantry_yaml.contains("container: demo-app"));
        assert!(gantry_yaml.contains("type: tcp"));

        // Dockerfile copied for build service
        assert!(out.join("services/app/Dockerfile").is_file());
        assert_eq!(
            fs::read_to_string(out.join("services/app/Dockerfile")).unwrap(),
            "FROM alpine"
        );

        // Template rendered
        let config_toml = fs::read_to_string(out.join("services/app/config.toml")).unwrap();
        assert!(config_toml.contains("port = 8080"));
        assert!(config_toml.contains("log_level = \"info\""));

        // Prebuilt service has no service directory content (only the dir itself)
        assert!(out.join("services/db").is_dir());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn generate_invalid_json() {
        let tmp = make_tmp("badjson");
        let result = generate_from_json("not json", "test", &tmp);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to parse"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn generate_missing_source_files() {
        let tmp = make_tmp("missingsrc");
        // Don't create the Dockerfile — validation should fail
        let result = generate_from_json(sample_json(), "test", &tmp);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
