use std::path::PathBuf;

use gantry::{api, cli, config, docker, error, graph, model};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "gantry", about = "Docker service orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the supervisor (runs inside docker-compose)
    Serve {
        #[arg(short, long, default_value = "/etc/gantry/config.yaml")]
        config: PathBuf,
        #[arg(short, long, default_value = "9090")]
        port: u16,
    },
    /// Show status
    Status {
        /// "service" or "target"
        kind: Option<String>,
        /// Name of the service or target
        name: Option<String>,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Stop a service
    Stop {
        service: String,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Start a service
    Start {
        service: String,
        #[arg(long, default_value = "60")]
        timeout: u64,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Restart a service
    Restart {
        service: String,
        #[arg(long, default_value = "120")]
        timeout: u64,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Replace a service (rebuild + recreate)
    Replace {
        service: String,
        #[arg(long, default_value = "120")]
        timeout: u64,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Converge a target to testable state
    Converge {
        target: String,
        #[arg(long, default_value = "120")]
        timeout: u64,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Reprobe a service or target
    Reprobe {
        /// "service" or "target"
        kind: String,
        /// Name
        name: String,
        #[arg(long, default_value = "60")]
        timeout: u64,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Reload config
    Reload {
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
    /// Create containers and start supervisor
    Up {
        #[arg(short, long, default_value = "http://localhost:9090")]
        host: String,
        /// Timeout in seconds to wait for supervisor to be ready
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// Show dependency graph
    Graph {
        /// Optional target name to filter
        target: Option<String>,
        #[arg(long, default_value = "http://localhost:9090")]
        host: String,
    },
}

#[tokio::main]
async fn main() {
    // RUST_LOG controls verbosity:
    //   info  (default): state changes, operations
    //   debug: + probe attempts with backoff timing
    //   trace: + staleness propagation, start_after waits
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gantry=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config, port } => {
            if let Err(e) = serve(config, port).await {
                eprintln!("fatal: {e}");
                std::process::exit(1);
            }
        }
        Commands::Status { kind, name, host } => {
            if let Err(e) = cli::status(&host, kind.as_deref(), name.as_deref()).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Stop { service, host } => {
            if let Err(e) = cli::post_op(&host, &format!("/api/stop/service/{service}"), None).await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Start {
            service,
            timeout,
            host,
        } => {
            if let Err(e) = cli::post_op(
                &host,
                &format!("/api/start/service/{service}"),
                Some(timeout),
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Restart {
            service,
            timeout,
            host,
        } => {
            if let Err(e) = cli::post_op(
                &host,
                &format!("/api/restart/service/{service}"),
                Some(timeout),
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Replace {
            service,
            timeout,
            host,
        } => {
            if let Err(e) = cli::post_op(
                &host,
                &format!("/api/replace/service/{service}"),
                Some(timeout),
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Converge {
            target,
            timeout,
            host,
        } => {
            if let Err(e) = cli::post_op(
                &host,
                &format!("/api/converge/target/{target}"),
                Some(timeout),
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Reprobe {
            kind,
            name,
            timeout,
            host,
        } => {
            if let Err(e) =
                cli::post_op(&host, &format!("/api/reprobe/{kind}/{name}"), Some(timeout)).await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Reload { host } => {
            if let Err(e) = cli::post_op(&host, "/api/reload", None).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Up { host, timeout } => {
            if let Err(e) = up(&host, timeout).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Graph { target, host } => {
            if let Err(e) = cli::graph(&host, target.as_deref()).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn up(host: &str, timeout: u64) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Step 1: docker compose up --no-start
    eprintln!("creating containers...");
    let output = tokio::process::Command::new("docker")
        .args(["compose", "up", "--no-start"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker compose up --no-start failed:\n{stderr}").into());
    }

    // Step 2: docker compose start gantry
    eprintln!("starting gantry supervisor...");
    let output = tokio::process::Command::new("docker")
        .args(["compose", "start", "gantry"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker compose start gantry failed:\n{stderr}").into());
    }

    // Step 3: wait for supervisor API
    eprintln!("waiting for supervisor API...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let url = format!("{host}/api/status");
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("timeout waiting for supervisor API".into());
        }
        match reqwest::get(&url).await {
            Ok(resp) if resp.status().is_success() => {
                eprintln!("supervisor ready");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

async fn serve(config_path: PathBuf, port: u16) -> error::Result<()> {
    tracing::info!("loading config from {}", config_path.display());
    let config = config::GantryConfig::load(&config_path)?;
    let dep_graph = graph::DependencyGraph::build(&config)?;

    let all_svcs: Vec<String> = config.services.keys().cloned().collect();
    let levels = dep_graph.topo_levels(&all_svcs);
    let levels_str: Vec<String> = levels
        .iter()
        .map(|l| format!("[{}]", l.join(", ")))
        .collect();
    tracing::info!(
        "{} services, {} targets, startup: {}",
        config.services.len(),
        config.targets.len(),
        levels_str.join(" → "),
    );

    let mut runtime = model::RuntimeState::from_config(&config);

    // Flatten target probes
    for (tgt_name, tgt) in runtime.targets.iter_mut() {
        tgt.transitive_probes = dep_graph.flatten_target(tgt_name, &config);
    }

    let docker_client = docker::DockerClient::connect()?;
    tracing::info!("connected to docker");

    // Check initial container states
    for (svc_name, svc) in runtime.services.iter_mut() {
        match docker_client.inspect_container(&svc.container).await {
            Ok(Some(info)) => {
                if info.running {
                    svc.state = model::ServiceState::Running;
                    tracing::info!("[{svc_name}] running");
                } else if info.exit_code != 0 {
                    svc.state = model::ServiceState::Crashed;
                    tracing::info!("[{svc_name}] crashed (exit {})", info.exit_code);
                } else {
                    tracing::info!("[{svc_name}] stopped");
                }
            }
            Ok(None) => {
                tracing::info!("[{svc_name}] container '{}' not found", svc.container);
            }
            Err(e) => {
                tracing::info!("{svc_name}: failed to inspect: {e}");
            }
        }
    }

    let app_state = api::AppState::new(config, dep_graph, runtime, docker_client, config_path);
    let router = api::routes::router(app_state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
