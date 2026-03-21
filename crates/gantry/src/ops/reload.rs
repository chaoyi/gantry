use std::path::Path;

use crate::api::AppState;
use crate::config::GantryConfig;
use crate::error::Result;
use crate::graph::DependencyGraph;
use crate::model::RuntimeState;
use crate::ops::OpResponse;

pub async fn reload(state: &AppState, config_path: &Path) -> Result<OpResponse> {
    let new_config = GantryConfig::load(config_path)?;
    let new_graph = DependencyGraph::build(&new_config)?;
    let new_runtime = RuntimeState::from_config(&new_config);

    // Update flattened target probes
    let mut targets = new_runtime.targets;
    for (tgt_name, tgt) in targets.iter_mut() {
        tgt.transitive_probes = new_graph.flatten_target(tgt_name, &new_config);
    }

    *state.config.write().await = new_config;
    *state.graph.write().await = new_graph;
    // Preserve running service states where possible
    // For now, reset everything
    *state.services.write().await = new_runtime.services;
    *state.targets.write().await = targets;

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: 0,
        error: None,
        actions: Default::default(),
        probes: Default::default(),
        targets: Default::default(),
    })
}
