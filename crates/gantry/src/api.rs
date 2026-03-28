pub mod routes;
pub mod state;
pub mod ws;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GantryConfig;
use crate::docker::DockerClient;
use crate::events::EventBus;
use crate::graph::DependencyGraph;
use crate::model::{RuntimeState, ServiceRuntime, TargetRuntime};
use crate::ops::OpLock;

/// Lock ordering to prevent deadlocks:
///   1. Never hold services.write + targets.read/write simultaneously
///   2. services.read + targets.write is OK (emit_target_states)
///   3. When both services and targets are needed for writes, read one first,
///      drop it, then write the other (see emit_svc_display_states).
///
/// `graph` and `config` are immutable after startup — no lock needed.
pub struct AppState {
    pub services: RwLock<indexmap::IndexMap<String, ServiceRuntime>>,
    pub targets: RwLock<indexmap::IndexMap<String, TargetRuntime>>,
    pub graph: Arc<DependencyGraph>,
    pub config: Arc<GantryConfig>,
    pub docker: DockerClient,
    pub op_lock: Arc<OpLock>,
    pub events: EventBus,
}

impl AppState {
    pub fn new(
        config: GantryConfig,
        graph: DependencyGraph,
        runtime: RuntimeState,
        docker: DockerClient,
    ) -> Arc<Self> {
        Arc::new(Self {
            services: RwLock::new(runtime.services),
            targets: RwLock::new(runtime.targets),
            graph: Arc::new(graph),
            config: Arc::new(config),
            docker,
            op_lock: OpLock::new(),
            events: EventBus::new(1024),
        })
    }
}
