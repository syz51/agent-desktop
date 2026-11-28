#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(target_os = "macos")]
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
#[command(name = "desktopd", version, about = "Persistent agent-desktop daemon")]
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
    /// Linux: launch a private daemon-owned headless Xvfb virtual display (24/7 server mode).
    #[arg(long, env = "DESKTOP_XVFB")]
    xvfb: bool,
    /// Linux: virtual screen resolution when --xvfb is set (e.g. 1920x1080 or 1280x800).
    #[arg(long, env = "DESKTOP_XVFB_SIZE", default_value = "1920x1080")]
    xvfb_size: String,
    /// Linux: explicit X11 DISPLAY override (e.g. :0 or :99).
    #[arg(long, env = "DESKTOP_DISPLAY")]
    display: Option<String>,
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
        let control_active = Arc::new(AtomicBool::new(false));
        let kill_switch = Arc::new(desktop_core::EscKillSwitch::new(stop_flag, control_active));
        let ui = desktop_macos::UiHandle::new(Arc::clone(&kill_switch));
        let ui_shutdown = ui.clone();
        let backend = Arc::new(desktop_macos::MacOsBackend::with_ui(
            Arc::clone(&kill_switch),
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
        desktop_macos::run_main_loop(kill_switch);
    }

    #[cfg(target_os = "linux")]
    {
        let (width, height) = desktop_linux::xvfb::parse_xvfb_size(&args.xvfb_size)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cfg = desktop_linux::LinuxConfig {
            display: args.display,
            xvfb: args.xvfb,
            xvfb_config: desktop_linux::xvfb::XvfbConfig {
                width,
                height,
                ..desktop_linux::xvfb::XvfbConfig::default()
            },
        };
        let backend = Arc::new(
            desktop_linux::LinuxBackend::connect(cfg).map_err(|e| anyhow::anyhow!("{e}"))?,
        );
        let engine = Arc::new(DesktopEngine::new(
            backend,
            Arc::new(SystemClock),
            PolicyStore::new(policy_path),
            AuditLog::new(&state_dir),
        ));
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_with_signals(socket, fleet_path, engine, None))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (socket, state_dir, policy_path, fleet_path, args);
        anyhow::bail!("agent-desktop daemon is currently supported on macOS and Linux");
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

    // Periodic lease reaper so abandoned remote HTTP sessions release WarmCaptureStream,
    // overlay, and PreventUserIdleDisplaySleep promptly on TTL expiry even while idle.
    let reaper_engine = Arc::clone(&engine);
    let mut reaper_rx = rx.clone();
    let reaper_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = reaper_rx.changed() => {
                    if *reaper_rx.borrow() {
                        break;
                    }
                }
                _ = tick.tick() => {
                    let eng = Arc::clone(&reaper_engine);
                    let _ = tokio::task::spawn_blocking(move || eng.reap_expired_lease()).await;
                }
            }
        }
    });

    let res = serve_uds(socket, engine, rx).await;
    let _ = tx.send(true);
    let _ = reaper_task.await;
    if let Some(task) = remote_task {
        let _ = task.await;
    }
    #[cfg(target_os = "macos")]
    if let Some(ui) = ui {
        ui.shutdown();
    }
    res
}
