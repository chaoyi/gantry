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

    pub async fn inspect_container(&self, name: &str) -> Result<Option<ContainerInfo>> {
        match self.docker.inspect_container(name, None).await {
            Ok(info) => {
                let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                let exit_code = info.state.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
                let started_at = info
                    .state
                    .as_ref()
                    .and_then(|s| s.started_at.as_deref())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.timestamp())
                    .unwrap_or(0);
                Ok(Some(ContainerInfo {
                    running,
                    exit_code,
                    started_at,
                }))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Debug)]
pub struct ContainerInfo {
    pub running: bool,
    pub exit_code: i64,
    pub started_at: i64,
}
