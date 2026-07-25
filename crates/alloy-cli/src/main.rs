//! Alloy CLI (`alloy` binary) — RFC-0001 stub.
//!
//! Author: arkadianet

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy_runtime::{AlloyRuntime, ConfigPaths, RuntimeConfig, RuntimePhase};
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

    // Active router.toml (not .example); ALLOY_PROFILE / ALLOY_ROUTER / ALLOY_DATA_DIR honored.
    let paths = ConfigPaths::for_workspace(workspace);
    let cfg = RuntimeConfig::load(paths)?;
    let mut rt = AlloyRuntime::new();
    rt.configure(cfg)?;

    // Arm SIGINT/SIGTERM → cancellation before start so startup I/O can abort.
    let cancel = rt.handle().cancellation();
    let signal_task = tokio::spawn(async move {
        if wait_for_shutdown_signal().await.is_ok() {
            cancel.cancel();
        }
    });

    match rt.start().await {
        Ok(handle) => {
            tracing::info!(phase = ?handle.phase(), "runtime running; Ctrl-C / SIGTERM to stop");
            // Post-start: wait until the early-armed signal cancels the token.
            handle.cancellation().cancelled().await;
            let _ = signal_task.await;
            graceful_shutdown(rt, Duration::from_secs(10)).await?;
            Ok(())
        }
        Err(e) if rt.handle().cancellation().is_cancelled() => {
            tracing::info!("shutdown signal during start; draining");
            let _ = signal_task.await;
            graceful_shutdown(rt, Duration::from_secs(10)).await?;
            let _ = e;
            Ok(())
        }
        Err(e) => {
            signal_task.abort();
            Err(e.into())
        }
    }
}

/// Wait for SIGINT or SIGTERM (Unix). Shared so tests can call [`graceful_shutdown`] directly.
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                r?;
                tracing::info!("SIGINT received");
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received");
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        tracing::info!("SIGINT received");
        Ok(())
    }
}

/// Drain (when Running) then shutdown — production signal path and test seam.
async fn graceful_shutdown(
    rt: AlloyRuntime,
    grace: Duration,
) -> Result<(), alloy_runtime::RuntimeError> {
    if rt.handle().phase() == RuntimePhase::Running {
        rt.drain(grace).await?;
    }
    rt.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod signal_path {
    use super::*;

    #[tokio::test]
    async fn graceful_shutdown_helper() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
        std::fs::write(
            dir.path().join("profiles/default.toml"),
            include_str!("../../../profiles/default.toml"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("router.toml"),
            include_str!("../../../router.toml.example"),
        )
        .unwrap();
        std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();

        let paths = ConfigPaths::for_workspace(dir.path().to_path_buf());
        let cfg = RuntimeConfig::load(paths).unwrap();
        let mut rt = AlloyRuntime::new();
        rt.configure(cfg).unwrap();
        let _ = rt.start().await.unwrap();
        graceful_shutdown(rt, Duration::from_millis(50))
            .await
            .unwrap();
    }
}
