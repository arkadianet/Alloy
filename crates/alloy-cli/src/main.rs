//! Alloy CLI (`alloy` binary) — RFC-0001 stub.
//!
//! Author: arkadianet

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy_runtime::{AlloyRuntime, ConfigPaths, RuntimeConfig};
use clap::{Parser, Subcommand};
use tracing::error;

#[derive(Debug, Parser)]
#[command(name = "alloy", version, about = "Alloy AI Engineering Runtime", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the runtime host and wait for shutdown signal (lifecycle smoke).
    Host {
        /// Workspace root used to resolve `.alloy` and config files.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            // `--help` / `--version` handled by clap; bare invocation prints help-ish usage.
            println!("alloy {}", env!("CARGO_PKG_VERSION"));
            println!("Run `alloy --help` for usage.");
            ExitCode::SUCCESS
        }
        Some(Commands::Host { workspace }) => match run_host(workspace).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                error!(error = %e, "host failed");
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

async fn run_host(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    alloy_runtime::logging::init_tracing();

    let paths = ConfigPaths {
        profile: workspace.join("profiles/default.toml"),
        router: workspace.join("router.toml.example"),
        example_env: workspace.join("example.env"),
        data_dir: None,
        workspace_root: Some(workspace.clone()),
    };
    let cfg = RuntimeConfig::load(paths)?;
    let mut rt = AlloyRuntime::new();
    rt.configure(cfg)?;
    let handle = rt.start().await?;

    tracing::info!(phase = ?handle.phase(), "runtime running; press Ctrl-C to stop");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received");
        }
    }

    rt.drain(Duration::from_secs(10)).await?;
    rt.shutdown().await?;
    Ok(())
}

/// Shared helper used by integration tests to exercise drain→shutdown without signals.
#[cfg(test)]
mod signal_path {
    use super::*;

    #[tokio::test]
    async fn drain_shutdown_helper() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal config fixtures
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
        std::fs::write(
            dir.path().join("profiles/default.toml"),
            include_str!("../../../profiles/default.toml"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("router.toml.example"),
            include_str!("../../../router.toml.example"),
        )
        .unwrap();
        std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();

        let paths = ConfigPaths {
            profile: dir.path().join("profiles/default.toml"),
            router: dir.path().join("router.toml.example"),
            example_env: dir.path().join("example.env"),
            data_dir: None,
            workspace_root: Some(dir.path().to_path_buf()),
        };
        let cfg = RuntimeConfig::load(paths).unwrap();
        let mut rt = AlloyRuntime::new();
        rt.configure(cfg).unwrap();
        let _ = rt.start().await.unwrap();
        rt.drain(Duration::from_millis(50)).await.unwrap();
        rt.shutdown().await.unwrap();
    }
}
