use std::path::PathBuf;

use clap::Parser;
use gantry::{api, config, docker, error, graph, model, watcher};

#[derive(Parser)]
#[command(
    name = "gantry",
    about = "Dependency-aware startup and health probing for docker compose"
)]
struct Cli {
    #[arg(short, long, default_value = "/etc/gantry/config.yaml")]
    config: PathBuf,
    #[arg(short, long, default_value = "9090")]
    port: u16,
}

#[tokio::main]
async fn main() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gantry=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    if let Err(e) = serve(cli.config, cli.port).await {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn serve(config_path: PathBuf, port: u16) -> error::Result<()> {
    tracing::info!("loading config from {}", config_path.display());
    let config = config::GantryConfig::load(&config_path)?;
    let dep_graph = graph::DependencyGraph::build(&config)?;

    tracing::info!(
        "{} services, {} targets",
        config.services.len(),
        config.targets.len(),
    );

    let mut runtime = model::RuntimeState::from_config(&config);

    for (tgt_name, tgt) in runtime.targets.iter_mut() {
        tgt.transitive_probes = dep_graph.flatten_target(tgt_name, &config);
    }

    let docker_client = docker::DockerClient::connect()?;
    tracing::info!("connected to docker");

    for (svc_name, svc) in runtime.services.iter_mut() {
        match docker_client.inspect_container(&svc.container).await {
            Ok(Some(info)) => {
                if info.running {
                    svc.state = model::ServiceState::Running;
                    tracing::info!("svc [{svc_name}] running (container up)");
                } else if info.exit_code != 0 {
                    svc.state = model::ServiceState::Crashed;
                    tracing::info!("svc [{svc_name}] crashed (exit {})", info.exit_code);
                } else {
                    tracing::info!("svc [{svc_name}] stopped");
                }
            }
            Ok(None) => {
                tracing::info!("svc [{svc_name}] stopped (no container)");
            }
            Err(e) => {
                tracing::warn!("svc [{svc_name}] inspect error: {e}");
            }
        }
    }

    // Set initial probe states based on which containers are actually running.
    // Running services with all deps running get Pending(Unchecked) probes;
    // others stay Red.
    dep_graph.initialize_probe_states(&mut runtime.services);

    let app_state = api::AppState::new(config, dep_graph, runtime, docker_client);

    // Watch Docker events for external container start/stop
    tokio::spawn(watcher::watch_docker_events(app_state.clone()));

    let router = api::routes::router(app_state);
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
