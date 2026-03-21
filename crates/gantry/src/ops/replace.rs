use std::time::Duration;

use crate::api::AppState;
use crate::error::Result;
use crate::ops::OpResponse;

pub async fn replace(
    state: &AppState,
    service_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    // Build
    state.docker.compose_build(service_name).await?;

    // Stop if running
    let _ = super::stop::stop(state, service_name).await;

    // Remove + recreate
    let container_name = {
        let services = state.services.read().await;
        services[service_name].container.clone()
    };
    let _ = state.docker.remove_container(&container_name).await;
    state.docker.compose_up_no_start(service_name).await?;

    // Start + probe
    let mut response = super::start::start(state, service_name, timeout).await?;
    response.actions.rebuilt.push(service_name.to_string());
    response.actions.started.retain(|s| s != service_name);
    Ok(response)
}
