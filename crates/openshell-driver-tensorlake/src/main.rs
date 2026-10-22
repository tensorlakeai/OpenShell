// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use miette::{IntoDiagnostic, Result};
use tracing::info;

use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_tensorlake::config::{
    DEFAULT_API_URL, DEFAULT_CPUS, DEFAULT_GUARD_CPUS, DEFAULT_GUARD_MEMORY_MB, DEFAULT_MEMORY_MB,
    DEFAULT_PLATFORM_API_URL,
};
use openshell_driver_tensorlake::{
    ComputeDriverService, TensorlakeComputeConfig, TensorlakeComputeDriver,
};

const TRACING: openshell_otel::ComputeDriverTracing = openshell_otel::compute_driver_tracing!();

#[derive(Parser)]
#[command(name = "openshell-driver-tensorlake")]
#[command(version = VERSION)]
struct Args {
    /// Operator-owned JSON policy; omitted means driver config disabled.
    #[arg(
        long,
        env = "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON",
        default_value = "{}"
    )]
    admission_config_json: openshell_core::resource_admission::DriverAdmissionConfig,

    /// Unix socket that the gateway connects to.
    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_SOCKET")]
    bind_socket: PathBuf,

    #[arg(long, env = "OPENSHELL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[arg(long, env = "OPENSHELL_OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,

    #[arg(long, env = "OPENSHELL_GATEWAY_NAME")]
    gateway_name: Option<String>,

    /// Public gateway gRPC endpoint that guards in Tensorlake VMs dial.
    #[arg(long, env = "OPENSHELL_GRPC_ENDPOINT")]
    grpc_endpoint: String,

    /// PEM CA bundle that guards trust for the gateway. Required for https.
    #[arg(long, env = "OPENSHELL_TENSORLAKE_GATEWAY_TLS_CA")]
    gateway_tls_ca: Option<PathBuf>,

    #[arg(long, env = "TENSORLAKE_API_URL", default_value = DEFAULT_API_URL)]
    tensorlake_api_url: String,

    /// Tensorlake platform API. The tunnel in each guard VM checks its key
    /// there, so `--allow-out` adds this host.
    #[arg(
        long,
        env = "TENSORLAKE_PLATFORM_API_URL",
        default_value = DEFAULT_PLATFORM_API_URL
    )]
    tensorlake_platform_api_url: String,

    #[arg(long, env = "TENSORLAKE_API_KEY", hide_env_values = true)]
    tensorlake_api_key: String,

    /// Registered Tensorlake image with the `OpenShell` runtime and launcher.
    #[arg(long, env = "OPENSHELL_SANDBOX_IMAGE")]
    sandbox_image: String,

    /// Registered Tensorlake image for guard VMs. Defaults to the sandbox
    /// image, which contains the guard and the Tensorlake CLI.
    #[arg(long, env = "OPENSHELL_TENSORLAKE_GUARD_IMAGE")]
    guard_image: Option<String>,

    #[arg(long, env = "OPENSHELL_TENSORLAKE_GUARD_CPUS", default_value_t = DEFAULT_GUARD_CPUS)]
    guard_cpus: f64,

    #[arg(long, env = "OPENSHELL_TENSORLAKE_GUARD_MEMORY_MB", default_value_t = DEFAULT_GUARD_MEMORY_MB)]
    guard_memory_mb: i64,

    #[arg(long, env = "OPENSHELL_TENSORLAKE_CPUS", default_value_t = DEFAULT_CPUS)]
    cpus: f64,

    #[arg(long, env = "OPENSHELL_TENSORLAKE_MEMORY_MB", default_value_t = DEFAULT_MEMORY_MB)]
    memory_mb: i64,

    #[arg(long, env = "OPENSHELL_TENSORLAKE_DISK_MB")]
    disk_mb: Option<u64>,

    /// Tensorlake idle timeout in seconds. 0 uses the plan maximum. Guard
    /// traffic to the gateway is outbound, so Tensorlake counts a working
    /// sandbox as idle; a short timeout suspends it.
    #[arg(long, env = "OPENSHELL_TENSORLAKE_TIMEOUT_SECS", default_value_t = 0)]
    timeout_secs: i64,

    /// Tensorlake `allow_out` entry (repeatable, or comma-separated in the
    /// environment). The gateway host is added automatically. Empty keeps
    /// Tensorlake internet access on and relies on the `OpenShell` guard.
    #[arg(long, env = "OPENSHELL_TENSORLAKE_ALLOW_OUT", value_delimiter = ',')]
    allow_out: Vec<String>,

    #[arg(
        long,
        env = "OPENSHELL_TENSORLAKE_POLL_INTERVAL_SECS",
        default_value_t = 3
    )]
    poll_interval_secs: u64,

    /// Directory for a copy of each sandbox request. After a restart the
    /// driver reads it to report recovered sandboxes under their names.
    #[arg(
        long,
        env = "OPENSHELL_TENSORLAKE_STATE_DIR",
        default_value = "target/openshell-tensorlake-driver"
    )]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _tracing = openshell_otel::install_driver_tracing(
        TRACING,
        openshell_otel::DriverTracingConfig {
            endpoint: args.otlp_endpoint.as_deref(),
            gateway_name: args.gateway_name.as_deref(),
            service_version: VERSION,
            log_level: &args.log_level,
        },
    );

    let config = TensorlakeComputeConfig {
        api_url: args.tensorlake_api_url,
        platform_api_url: args.tensorlake_platform_api_url,
        api_key: args.tensorlake_api_key,
        default_image: args.sandbox_image,
        grpc_endpoint: args.grpc_endpoint,
        gateway_tls_ca: args.gateway_tls_ca,
        default_cpus: args.cpus,
        default_memory_mb: args.memory_mb,
        default_disk_mb: args.disk_mb,
        timeout_secs: Some(args.timeout_secs),
        allow_out: args
            .allow_out
            .into_iter()
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty())
            .collect(),
        poll_interval: Duration::from_secs(args.poll_interval_secs.max(1)),
        log_level: args.log_level,
        guard_image: args.guard_image,
        guard_cpus: args.guard_cpus,
        guard_memory_mb: args.guard_memory_mb,
        state_dir: args.state_dir,
    };
    let driver = TensorlakeComputeDriver::new(config, args.admission_config_json)
        .await
        .map_err(|error| miette::miette!("{error}"))?;

    let service = ComputeDriverServer::new(ComputeDriverService::new(driver));
    let listener = openshell_core::external_driver_socket::bind_private(&args.bind_socket)
        .map_err(|err| miette::miette!("{err}"))?;
    let _cleanup =
        openshell_core::external_driver_socket::SocketCleanup::new(args.bind_socket.clone());
    info!(socket = %args.bind_socket.display(), "Starting Tensorlake compute driver");
    tonic::transport::Server::builder()
        .layer(openshell_otel::compute_driver_rpc_layer())
        .add_service(service)
        .serve_with_incoming_shutdown(
            openshell_core::external_driver_socket::SameUidUnixIncoming::new(listener),
            shutdown_signal(),
        )
        .await
        .into_diagnostic()
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
    info!("Received shutdown signal, draining in-flight requests");
}
