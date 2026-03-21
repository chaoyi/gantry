use bollard::Docker;
use bollard::container::{StartContainerOptions, StopContainerOptions};

use crate::error::{GantryError, Result};

pub struct DockerClient {
    docker: Docker,
}

impl DockerClient {
    pub fn connect() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| GantryError::Docker(e.to_string()))?;
        Ok(Self { docker })
    }

    pub fn inner(&self) -> &Docker {
        &self.docker
    }

    pub async fn start_container(&self, name: &str) -> Result<()> {
        self.docker
            .start_container(name, None::<StartContainerOptions<String>>)
            .await?;
        Ok(())
    }

    pub async fn stop_container(&self, name: &str) -> Result<()> {
        let opts = StopContainerOptions { t: 10 };
        self.docker.stop_container(name, Some(opts)).await?;
        Ok(())
    }

    pub async fn remove_container(&self, name: &str) -> Result<()> {
        self.docker.remove_container(name, None).await?;
        Ok(())
    }

    pub async fn inspect_container(&self, name: &str) -> Result<Option<ContainerInfo>> {
        match self.docker.inspect_container(name, None).await {
            Ok(info) => {
                let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                let exit_code = info.state.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
                Ok(Some(ContainerInfo { running, exit_code }))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn compose_build(&self, service: &str) -> Result<String> {
        let output = tokio::process::Command::new("docker")
            .args(["compose", "build", service])
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            return Err(GantryError::Docker(format!(
                "docker compose build failed:\n{stderr}"
            )));
        }
        Ok(format!("{stdout}{stderr}"))
    }

    pub async fn compose_up_no_start(&self, service: &str) -> Result<()> {
        let output = tokio::process::Command::new("docker")
            .args(["compose", "up", "--no-start", service])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GantryError::Docker(format!(
                "docker compose up --no-start failed:\n{stderr}"
            )));
        }
        Ok(())
    }

    pub async fn compose_up_no_start_all(&self) -> Result<()> {
        let output = tokio::process::Command::new("docker")
            .args(["compose", "up", "--no-start"])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GantryError::Docker(format!(
                "docker compose up --no-start failed:\n{stderr}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ContainerInfo {
    pub running: bool,
    pub exit_code: i64,
}
