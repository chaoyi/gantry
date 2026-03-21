pub mod routes;
pub mod ws;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GantryConfig;
use crate::docker::DockerClient;
use crate::events::EventBus;
use crate::graph::DependencyGraph;
use crate::model::{RuntimeState, ServiceRuntime, TargetRuntime};
use crate::ops::OpLock;

pub struct AppState {
    pub services: RwLock<indexmap::IndexMap<String, ServiceRuntime>>,
    pub targets: RwLock<indexmap::IndexMap<String, TargetRuntime>>,
    pub graph: RwLock<DependencyGraph>,
    pub config: RwLock<GantryConfig>,
    pub docker: DockerClient,
    pub op_lock: Arc<OpLock>,
    pub events: EventBus,
    pub config_path: PathBuf,
}

impl AppState {
    pub fn new(
        config: GantryConfig,
        graph: DependencyGraph,
        runtime: RuntimeState,
        docker: DockerClient,
        config_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            services: RwLock::new(runtime.services),
            targets: RwLock::new(runtime.targets),
            graph: RwLock::new(graph),
            config: RwLock::new(config),
            docker,
            op_lock: OpLock::new(),
            events: EventBus::new(1024),
            config_path,
        })
    }
}
