//! Alloy CLI (`alloy` binary) — RFC-0015 composition root plus a terminal.
//!
//! `alloy-cli` parses argv, resolves config, builds every subsystem in one
//! honest order (§6.2), hands the object graph to the control plane, and
//! renders what the control plane returns. It contains no planning,
//! scheduling, retry, budget, or verification decision (rule B1).
//!
//! Author: arkadianet

#![forbid(unsafe_code)]

mod args;
mod assembly;
mod commands;
mod errx;
mod outfmt;
mod resolve;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy_runtime::{AlloyRuntime, ConfigPaths, RuntimeConfig, RuntimePhase};
use clap::Parser;
use tracing::error;

use crate::args::{Cli, Commands};
use crate::errx::{CliError, Exit};

fn main() -> ExitCode {
    // CL9 — argument parsing completes before any file, network, or process
    // I/O, so `--help` / `--version` work in a directory with no config.
    let cli = Cli::parse();

    let body = async move {
        match cli.command {
            None => {
                println!("alloy {}", env!("CARGO_PKG_VERSION"));
                println!("Run `alloy --help` for usage.");
                ExitCode::SUCCESS
            }
            Some(Commands::Host) => {
                // CL1 — preserved RFC-0001 behaviour.
                match run_host(cli.globals.workspace.clone()).await {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        error!(error = %e, "host failed");
                        eprintln!("error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            Some(command) => {
                init_tracing_for(&cli.globals);
                let json = cli.globals.json;
                let name = command_name(&command);
                match commands::dispatch(cli.globals, command).await {
                    Ok(exit) => ExitCode::from(exit.code()),
                    Err(e) => {
                        report_error(name, json, &e);
                        ExitCode::from(e.exit.code())
                    }
                }
            }
        }
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(body)
}

fn command_name(c: &Commands) -> &'static str {
    match c {
        Commands::Run(_) => "run",
        Commands::Review(_) => "review",
        Commands::Events(_) => "events",
        Commands::Approve(_) => "approve",
        Commands::Cancel(_) => "cancel",
        Commands::Resume(_) => "resume",
        Commands::Index(_) => "index",
        Commands::Host => "host",
    }
}

/// Failure rendering: human diagnostics on stderr always; with `--json` the
/// envelope (without config echo — resolution may be what failed) on stdout.
fn report_error(command: &str, json: bool, e: &CliError) {
    eprintln!("error: {e}");
    if json {
        let doc = outfmt::envelope(
            command,
            e.exit,
            None,
            serde_json::json!({ "error": e.message }),
        );
        println!("{doc}");
    }
}

/// CL7 — `--verbose` raises the tracing filter; `--quiet` never changes
/// stdout or the exit code (handlers consult it for stderr progress).
fn init_tracing_for(globals: &args::Globals) {
    if globals.verbose && std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var(
            "RUST_LOG",
            "alloy=debug,alloy_runtime=debug,alloy_tools=debug",
        );
    }
    // OUT1 — CLI diagnostics go to stderr; stdout is results only.
    alloy_runtime::logging::init_tracing_stderr();
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

/// CR14/CR15/CR17 — arm the signal task for long-running subcommands
/// (`run`, `resume`, `index`; CL8). First signal cancels the runtime token
/// and nothing else; a second signal during drain escalates to an immediate
/// exit with `EX_CANCELLED`.
/// Takes a `cancel` closure instead of the token type so this crate never
/// names the token crate directly (T9); callers pass a closure over
/// `RuntimeHandle::cancellation()`.
pub(crate) fn arm_signal_task(
    cancel: impl FnOnce() + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if wait_for_shutdown_signal().await.is_ok() {
            cancel();
        }
        if wait_for_shutdown_signal().await.is_ok() {
            tracing::warn!("second signal: escalating past drain");
            std::process::exit(i32::from(Exit::Cancelled.code()));
        }
    })
}

/// Drain (when Running) then shutdown — production signal path and test seam.
pub(crate) async fn graceful_shutdown(
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

    /// The default log filter must cover *this* binary's tracing target.
    ///
    /// `module_path!()`'s root is the crate name the compiler assigned, which for a
    /// `[[bin]]` target is its `name` (`alloy`) — not the package name (`alloy-cli`).
    /// A filter naming `alloy_cli` silently drops every CLI log line, including
    /// "SIGTERM received" and the `host failed` error.
    #[test]
    fn default_log_filter_covers_this_binarys_tracing_target() {
        let target = module_path!().split("::").next().unwrap();
        let directive = format!("{target}=");
        assert!(
            alloy_runtime::logging::DEFAULT_FILTER.contains(&directive),
            "DEFAULT_FILTER {:?} has no directive for this binary's tracing target {:?}",
            alloy_runtime::logging::DEFAULT_FILTER,
            target,
        );
    }

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
