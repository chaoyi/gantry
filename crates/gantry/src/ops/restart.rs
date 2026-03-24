use std::time::Duration;

use crate::api::AppState;
use crate::error::Result;
use crate::ops::OpResponse;

pub async fn restart(
    state: &AppState,
    service_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    // Stop then start
    let _stop_result = super::stop::stop(state, service_name).await?;
    let start_result = super::start::start(state, service_name, timeout, true).await?;

    let mut response = start_result;
    response.actions.restarted.push(service_name.to_string());
    // Remove from started since it was a restart
    response.actions.started.retain(|s| s != service_name);
    Ok(response)
}
