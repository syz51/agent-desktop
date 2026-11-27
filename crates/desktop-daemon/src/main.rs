#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use clap::Parser;
use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
use desktop_daemon::paths::{
    default_fleet_path, default_policy_path, default_socket_path, default_state_dir,
};
use desktop_daemon::remote::{bind_http, serve_http_listener};
use desktop_daemon::server::serve_uds;
use desktop_proto::FleetConfig;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "desktopd", about = "Persistent agent-desktop daemon")]
struct Args {
    /// Unix domain socket path to listen on.
    #[arg(long, env = "DESKTOP_SOCKET")]
    socket: Option<PathBuf>,
    /// State directory for audit logs.
    #[arg(long, env = "DESKTOP_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Optional policy.toml path.
    #[arg(long, env = "DESKTOP_POLICY")]
    policy: Option<PathBuf>,
    /// Optional fleet.toml path (enables Tailscale HTTP RPC when `[listen]` is set).
    #[arg(long, env = "DESKTOP_FLEET")]
    fleet: Option<PathBuf>,
    /// Run without starting the macOS AppKit NSApplication overlay loop (for tests/CI).
    #[arg(long)]
    headless: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
    let policy_path = args.policy.unwrap_or_else(default_policy_path);
    let fleet_path = args.fleet.unwrap_or_else(default_fleet_path);

    #[cfg(target_os = "macos")]
    {
        if args.headless {
            let backend = Arc::new(desktop_macos::MacOsBackend::headless());
            let engine = Arc::new(DesktopEngine::new(
                backend,
                Arc::new(SystemClock),
                PolicyStore::new(policy_path),
                AuditLog::new(&state_dir),
            ));
            let rt = tokio::runtime::Runtime::new()?;
            return rt.block_on(run_with_signals(socket, fleet_path, engine, None));
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let ui = desktop_macos::UiHandle::new(Arc::clone(&stop_flag));
        let ui_shutdown = ui.clone();
        let backend = Arc::new(desktop_macos::MacOsBackend::with_ui(
            Arc::clone(&stop_flag),
            ui,
        ));
        let engine = Arc::new(DesktopEngine::new(
            backend,
            Arc::new(SystemClock),
            PolicyStore::new(policy_path),
            AuditLog::new(&state_dir),
        ));

        std::thread::Builder::new()
            .name("desktopd-tokio".into())
            .spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                if let Err(err) = rt.block_on(run_with_signals(
                    socket,
                    fleet_path,
                    engine,
                    Some(ui_shutdown.clone()),
                )) {
                    tracing::error!(%err, "desktopd server exited with error");
                }
                ui_shutdown.shutdown();
            })?;

        // AppKit runloop owns the process main thread on macOS.
        desktop_macos::run_main_loop(stop_flag);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (socket, state_dir, policy_path, fleet_path);
        anyhow::bail!("Linux backend (Phase 3) is not yet enabled in desktopd");
    }
}

async fn run_with_signals(
    socket: PathBuf,
    fleet_path: PathBuf,
    engine: Arc<DesktopEngine>,
    #[cfg(target_os = "macos")] ui: Option<desktop_macos::UiHandle>,
    #[cfg(not(target_os = "macos"))] _ui: Option<()>,
) -> Result<()> {
    let (tx, rx) = watch::channel(false);
    let sig_tx = tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sig_tx.send(true);
    });

    // Optional Tailscale/loopback remote HTTP RPC listener if `[listen]` is configured in fleet.toml.
    let remote_task = match FleetConfig::load_from(&fleet_path) {
        Ok(fleet) => {
            if let Some(listen) = fleet.listen {
                match bind_http(&listen, Arc::clone(&engine)).await {
                    Ok((listener, addr, router)) => {
                        tracing::info!(%addr, "bound Tailscale remote listener");
                        let remote_rx = rx.clone();
                        Some(tokio::spawn(async move {
                            if let Err(err) = serve_http_listener(listener, router, remote_rx).await
                            {
                                tracing::error!(%err, "remote HTTP listener exited");
                            }
                        }))
                    }
                    Err(err) => {
                        tracing::warn!(%err, "could not start remote Tailscale listener; continuing local UDS only");
                        None
                    }
                }
            } else {
                None
            }
        }
        Err(err) => {
            tracing::warn!(%err, path = %fleet_path.display(), "invalid fleet.toml; continuing local UDS only");
            None
        }
    };

    let res = serve_uds(socket, engine, rx).await;
    if let Some(task) = remote_task {
        let _ = tx.send(true);
        let _ = task.await;
    }
    #[cfg(target_os = "macos")]
    if let Some(ui) = ui {
        ui.shutdown();
    }
    res
}
