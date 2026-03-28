use std::path::Path;

use crate::error::{GantryError, Result};

use super::schema::{ImageDef, SetupJson};

/// Validate that all source files referenced in the setup exist on disk.
/// Collects all errors and reports them together.
pub fn validate(setup: &SetupJson, base_dir: &Path) -> Result<()> {
    let mut errors = Vec::new();

    for (svc_name, svc) in &setup.services {
        // Check Dockerfile for build images
        if let ImageDef::Build { ref build } = svc.image {
            let dockerfile = base_dir.join(&build.context).join(&build.dockerfile);
            if !dockerfile.is_file() {
                errors.push(format!(
                    "service '{svc_name}': Dockerfile not found: {}",
                    dockerfile.display()
                ));
            }
        }

        // Check file sources
        for file in &svc.files {
            let src_path = base_dir.join(&file.src);
            if file.src.ends_with('/') {
                if !src_path.is_dir() {
                    errors.push(format!(
                        "service '{svc_name}': directory not found: {}",
                        src_path.display()
                    ));
                }
            } else {
                if !src_path.is_file() {
                    errors.push(format!(
                        "service '{svc_name}': file not found: {}",
                        src_path.display()
                    ));
                }
                if file.template && src_path.is_dir() {
                    errors.push(format!(
                        "service '{svc_name}': template source must be a file, not directory: {}",
                        src_path.display()
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(GantryError::Validation(errors.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::schema::*;
    use indexmap::IndexMap;
    use std::fs;

    fn minimal_setup() -> SetupJson {
        SetupJson {
            name: "test".into(),
            services: IndexMap::new(),
            targets: IndexMap::new(),
            defaults: None,
            files: vec![],
            volumes: IndexMap::new(),
        }
    }

    #[test]
    fn validate_empty_setup() {
        let tmp = std::env::temp_dir().join("gantry_test_validate_empty");
        let _ = fs::create_dir_all(&tmp);
        assert!(validate(&minimal_setup(), &tmp).is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_missing_dockerfile() {
        let tmp = std::env::temp_dir().join("gantry_test_validate_dockerfile");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let mut services = IndexMap::new();
        services.insert(
            "app".into(),
            ServiceDef {
                container_name: "test-app".into(),
                config: None,
                image: ImageDef::Build {
                    build: BuildDef {
                        context: ".".into(),
                        dockerfile: "Dockerfile".into(),
                    },
                },
                env: IndexMap::new(),
                ports: vec![],
                volumes: vec![],
                command: None,
                entrypoint: None,
                user: None,
                working_dir: None,
                hostname: None,
                cap_add: vec![],
                cap_drop: vec![],
                privileged: None,
                labels: IndexMap::new(),
                extra_hosts: vec![],
                init: None,
                stop_grace_period: None,
                files: vec![],
                start_after: vec![],
                restart_on_fail: None,
                probes: IndexMap::new(),
            },
        );
        let setup = SetupJson {
            name: "test".into(),
            services,
            targets: IndexMap::new(),
            defaults: None,
            files: vec![],
            volumes: IndexMap::new(),
        };

        let err = validate(&setup, &tmp).unwrap_err();
        assert!(err.to_string().contains("Dockerfile not found"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_missing_file_source() {
        let tmp = std::env::temp_dir().join("gantry_test_validate_filesrc");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let mut services = IndexMap::new();
        services.insert(
            "app".into(),
            ServiceDef {
                container_name: "test-app".into(),
                config: None,
                image: ImageDef::Prebuilt("nginx".into()),
                env: IndexMap::new(),
                ports: vec![],
                volumes: vec![],
                command: None,
                entrypoint: None,
                user: None,
                working_dir: None,
                hostname: None,
                cap_add: vec![],
                cap_drop: vec![],
                privileged: None,
                labels: IndexMap::new(),
                extra_hosts: vec![],
                init: None,
                stop_grace_period: None,
                files: vec![FileDef {
                    src: "nonexistent.toml".into(),
                    dst: "config.toml".into(),
                    template: false,
                    context: None,
                }],
                start_after: vec![],
                restart_on_fail: None,
                probes: IndexMap::new(),
            },
        );
        let setup = SetupJson {
            name: "test".into(),
            services,
            targets: IndexMap::new(),
            defaults: None,
            files: vec![],
            volumes: IndexMap::new(),
        };

        let err = validate(&setup, &tmp).unwrap_err();
        assert!(err.to_string().contains("file not found"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let tmp = std::env::temp_dir().join("gantry_test_validate_multi");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let mut services = IndexMap::new();
        services.insert(
            "app".into(),
            ServiceDef {
                container_name: "test-app".into(),
                config: None,
                image: ImageDef::Build {
                    build: BuildDef {
                        context: ".".into(),
                        dockerfile: "Dockerfile".into(),
                    },
                },
                env: IndexMap::new(),
                ports: vec![],
                volumes: vec![],
                command: None,
                entrypoint: None,
                user: None,
                working_dir: None,
                hostname: None,
                cap_add: vec![],
                cap_drop: vec![],
                privileged: None,
                labels: IndexMap::new(),
                extra_hosts: vec![],
                init: None,
                stop_grace_period: None,
                files: vec![
                    FileDef {
                        src: "missing1.txt".into(),
                        dst: "a.txt".into(),
                        template: false,
                        context: None,
                    },
                    FileDef {
                        src: "missing2.txt".into(),
                        dst: "b.txt".into(),
                        template: false,
                        context: None,
                    },
                ],
                start_after: vec![],
                restart_on_fail: None,
                probes: IndexMap::new(),
            },
        );
        let setup = SetupJson {
            name: "test".into(),
            services,
            targets: IndexMap::new(),
            defaults: None,
            files: vec![],
            volumes: IndexMap::new(),
        };

        let err = validate(&setup, &tmp).unwrap_err();
        let msg = err.to_string();
        // Dockerfile + 2 missing files = 3 errors
        assert_eq!(msg.matches('\n').count(), 2, "expected 3 errors: {msg}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_all_present() {
        let tmp = std::env::temp_dir().join("gantry_test_validate_ok");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("migrations")).unwrap();
        fs::write(tmp.join("Dockerfile"), "FROM alpine").unwrap();
        fs::write(tmp.join("config.tmpl"), "port = {{ port }}").unwrap();

        let mut services = IndexMap::new();
        services.insert(
            "app".into(),
            ServiceDef {
                container_name: "test-app".into(),
                config: None,
                image: ImageDef::Build {
                    build: BuildDef {
                        context: ".".into(),
                        dockerfile: "Dockerfile".into(),
                    },
                },
                env: IndexMap::new(),
                ports: vec![],
                volumes: vec![],
                command: None,
                entrypoint: None,
                user: None,
                working_dir: None,
                hostname: None,
                cap_add: vec![],
                cap_drop: vec![],
                privileged: None,
                labels: IndexMap::new(),
                extra_hosts: vec![],
                init: None,
                stop_grace_period: None,
                files: vec![
                    FileDef {
                        src: "config.tmpl".into(),
                        dst: "config.toml".into(),
                        template: true,
                        context: None,
                    },
                    FileDef {
                        src: "migrations/".into(),
                        dst: "migrations/".into(),
                        template: false,
                        context: None,
                    },
                ],
                start_after: vec![],
                restart_on_fail: None,
                probes: IndexMap::new(),
            },
        );
        let setup = SetupJson {
            name: "test".into(),
            services,
            targets: IndexMap::new(),
            defaults: None,
            files: vec![],
            volumes: IndexMap::new(),
        };

        assert!(validate(&setup, &tmp).is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }
}
