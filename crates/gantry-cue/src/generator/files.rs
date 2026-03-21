use std::path::Path;

use crate::error::{GantryError, Result};

use super::schema::{BuildDef, FileDef};

/// Copy the entire build context directory into the output service directory.
pub fn copy_build_context(build: &BuildDef, base_dir: &Path, output_svc_dir: &Path) -> Result<()> {
    let context_dir = base_dir.join(&build.context);
    for entry in std::fs::read_dir(&context_dir)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = output_svc_dir.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Process file entries for a service: copy static files, render templates.
pub fn process_files(files: &[FileDef], base_dir: &Path, output_svc_dir: &Path) -> Result<()> {
    for file in files {
        let src_path = base_dir.join(&file.src);
        let dst_path = output_svc_dir.join(&file.dst);

        // Ensure parent directory exists
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if file.template {
            render_template(&src_path, &dst_path, file.context.as_ref())?;
        } else if file.src.ends_with('/') {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn render_template(src: &Path, dst: &Path, context: Option<&serde_json::Value>) -> Result<()> {
    let template_content = std::fs::read_to_string(src)?;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("template", &template_content)
        .map_err(|e| {
            GantryError::Config(format!("template parse error in {}: {e}", src.display()))
        })?;

    let ctx = match context {
        Some(val) => tera::Context::from_value(val.clone())
            .map_err(|e| GantryError::Config(format!("template context error: {e}")))?,
        None => tera::Context::new(),
    };

    let rendered = tera.render("template", &ctx).map_err(|e| {
        GantryError::Config(format!("template render error in {}: {e}", src.display()))
    })?;

    std::fs::write(dst, rendered)?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn make_tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gantry_test_files_{name}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn template_rendering() {
        let tmp = make_tmp("tmpl");
        let src_dir = tmp.join("src");
        let out_dir = tmp.join("out");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();

        fs::write(
            src_dir.join("config.toml.tmpl"),
            "[server]\nport = {{ port }}\nlog_level = \"{{ log_level }}\"\n",
        )
        .unwrap();

        let files = vec![FileDef {
            src: "config.toml.tmpl".into(),
            dst: "config.toml".into(),
            template: true,
            context: Some(json!({"port": 8080, "log_level": "info"})),
        }];

        process_files(&files, &src_dir, &out_dir).unwrap();

        let content = fs::read_to_string(out_dir.join("config.toml")).unwrap();
        assert!(content.contains("port = 8080"));
        assert!(content.contains("log_level = \"info\""));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn static_file_copy() {
        let tmp = make_tmp("static");
        let src_dir = tmp.join("src");
        let out_dir = tmp.join("out");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();

        fs::write(src_dir.join("readme.txt"), "hello").unwrap();

        let files = vec![FileDef {
            src: "readme.txt".into(),
            dst: "readme.txt".into(),
            template: false,
            context: None,
        }];

        process_files(&files, &src_dir, &out_dir).unwrap();

        assert_eq!(
            fs::read_to_string(out_dir.join("readme.txt")).unwrap(),
            "hello"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn directory_copy() {
        let tmp = make_tmp("dircp");
        let src_dir = tmp.join("src");
        let out_dir = tmp.join("out");
        fs::create_dir_all(src_dir.join("migrations/sub")).unwrap();
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(src_dir.join("migrations/001.sql"), "CREATE TABLE t;").unwrap();
        fs::write(src_dir.join("migrations/sub/002.sql"), "ALTER TABLE t;").unwrap();

        let files = vec![FileDef {
            src: "migrations/".into(),
            dst: "migrations/".into(),
            template: false,
            context: None,
        }];

        process_files(&files, &src_dir, &out_dir).unwrap();

        assert!(out_dir.join("migrations/001.sql").is_file());
        assert!(out_dir.join("migrations/sub/002.sql").is_file());
        assert_eq!(
            fs::read_to_string(out_dir.join("migrations/001.sql")).unwrap(),
            "CREATE TABLE t;"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nested_dst_directory_created() {
        let tmp = make_tmp("nested");
        let src_dir = tmp.join("src");
        let out_dir = tmp.join("out");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(src_dir.join("app.conf"), "setting=1").unwrap();

        let files = vec![FileDef {
            src: "app.conf".into(),
            dst: "config/app.conf".into(),
            template: false,
            context: None,
        }];

        process_files(&files, &src_dir, &out_dir).unwrap();
        assert!(out_dir.join("config/app.conf").is_file());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bad_template_returns_error() {
        let tmp = make_tmp("badtmpl");
        let src_dir = tmp.join("src");
        let out_dir = tmp.join("out");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();

        fs::write(src_dir.join("bad.tmpl"), "{{ unclosed").unwrap();

        let files = vec![FileDef {
            src: "bad.tmpl".into(),
            dst: "out.txt".into(),
            template: true,
            context: None,
        }];

        let err = process_files(&files, &src_dir, &out_dir);
        assert!(err.is_err());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn build_context_copy() {
        let tmp = make_tmp("buildctx");
        let base = tmp.join("base");
        let out = tmp.join("out");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(base.join("Dockerfile"), "FROM alpine").unwrap();

        let build = BuildDef {
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
        };
        copy_build_context(&build, &base, &out).unwrap();

        assert_eq!(
            fs::read_to_string(out.join("Dockerfile")).unwrap(),
            "FROM alpine"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
