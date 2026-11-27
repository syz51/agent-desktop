#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand, ValueEnum};
use desktop::client::{DesktopClient, default_fleet_path, default_socket_path};
use desktop_proto::{
    ActParams, ActionStep, DEFAULT_REMOTE_PORT, FindParams, FocusMode, ImageFormat,
    ListWindowsParams, MenuParams, PolicyMode, PostAction, ReadTextMode, ReadTextParams, RpcMethod,
    RpcResult, ScreenshotParams, ScreenshotTarget, SnapshotParams, SnapshotRenderOptions,
};
use rand::RngExt as _;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "desktop",
    version,
    about = "Fast Rust computer-use CLI and MCP server"
)]
struct Cli {
    /// Target host ("local" or configured Tailscale peer name).
    #[arg(long, global = true, default_value = "local")]
    host: String,
    /// Override local desktopd Unix socket path.
    #[arg(long, global = true, env = "DESKTOP_SOCKET")]
    socket: Option<PathBuf>,
    /// Override fleet.toml path.
    #[arg(long, global = true, env = "DESKTOP_FLEET")]
    fleet: Option<PathBuf>,
    /// Emit raw JSON output where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the thin stdio MCP server (12 lean tools).
    Mcp,
    /// List local and configured Tailscale fleet hosts with live status and latency.
    Hosts,
    /// Initialize or inspect Tailscale fleet config and bearer token.
    Fleet {
        #[command(subcommand)]
        action: FleetAction,
    },
    /// Show daemon status, TCC permissions, screen state, and active lease.
    Status,
    /// List active displays and visible windows.
    Windows {
        #[arg(long)]
        app: Option<String>,
    },
    /// Take a compact Accessibility tree snapshot with @eN element refs.
    Snapshot {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        max_depth: Option<usize>,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long, value_enum, default_value = "auto")]
        focus: CliFocusMode,
        /// Include per-element `[x,y wxh]` pixel bounds in compact output.
        #[arg(long)]
        bounds: bool,
    },
    /// Search UI elements in the Accessibility tree.
    Find {
        query: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Capture a screenshot of a display, window, app, or region.
    Screenshot {
        #[arg(long)]
        display: Option<u32>,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        annotate: bool,
        #[arg(long, value_enum, default_value = "jpeg")]
        format: CliImageFormat,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "auto")]
        focus: CliFocusMode,
    },
    /// Execute one or more UI action steps from a JSON array.
    Act {
        /// JSON array of ActionStep objects (e.g. '[{"op":"key","combo":"Return"}]').
        steps_json: String,
        #[arg(long, value_enum, default_value = "none")]
        after: CliPostAction,
        #[arg(long, value_enum, default_value = "auto")]
        focus: CliFocusMode,
        /// Release the control lease after the batch completes.
        #[arg(long)]
        release_after: bool,
    },
    /// Read text from an element ref, selection, or clipboard.
    ReadText {
        #[arg(long = "ref")]
        ref_id: Option<String>,
        #[arg(long, value_enum, default_value = "value")]
        mode: CliReadTextMode,
    },
    /// Invoke a native application menu-bar item by path (e.g. `desktop menu Notes File "New Note"`).
    Menu {
        app: String,
        #[arg(required = true)]
        path: Vec<String>,
    },
    /// Immediately stop any active desktop control session.
    Stop,
    /// Show or update opt-in policy mode.
    Policy {
        #[command(subcommand)]
        action: Option<PolicyAction>,
    },
    /// Ask macOS to register AgentDesktop.app in Accessibility & Screen Recording lists.
    Grant,
    /// Diagnose daemon connectivity, permissions, lock state, and AgentDesktop.app signing.
    Doctor,
    /// Benchmark end-to-end latency for status, windows, snapshot, screenshot, and find.
    Bench {
        /// Number of timed iterations per operation.
        #[arg(long, default_value_t = 10)]
        iterations: usize,
        /// Optional app filter for snapshot/find benchmarks.
        #[arg(long)]
        app: Option<String>,
    },
    /// Profile read-only MCP token & byte overhead across schema, discovery, perception, and action stages.
    Profile {
        /// Optional app filter for list_windows, snapshot, and find profiling.
        #[arg(long)]
        app: Option<String>,
        /// Optional query string for the find stage (default: "New").
        #[arg(long)]
        query: Option<String>,
        /// Also capture a background annotated screenshot to include screenshot metadata & image KB.
        #[arg(long)]
        screenshot: bool,
    },
}

#[derive(Debug, Subcommand)]
enum FleetAction {
    /// Generate a 0600 bearer token and starter Tailscale fleet.toml on this machine.
    Init {
        #[arg(long, default_value_t = DEFAULT_REMOTE_PORT)]
        port: u16,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PolicyAction {
    Show,
    SetMode {
        #[arg(value_enum)]
        mode: CliPolicyMode,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliFocusMode {
    Auto,
    Background,
    Foreground,
}

impl From<CliFocusMode> for FocusMode {
    fn from(v: CliFocusMode) -> Self {
        match v {
            CliFocusMode::Auto => Self::Auto,
            CliFocusMode::Background => Self::Background,
            CliFocusMode::Foreground => Self::Foreground,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliImageFormat {
    Jpeg,
    Png,
}

impl From<CliImageFormat> for ImageFormat {
    fn from(v: CliImageFormat) -> Self {
        match v {
            CliImageFormat::Jpeg => Self::Jpeg,
            CliImageFormat::Png => Self::Png,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPostAction {
    None,
    Snapshot,
    Screenshot,
    Annotated,
}

impl From<CliPostAction> for PostAction {
    fn from(v: CliPostAction) -> Self {
        match v {
            CliPostAction::None => Self::None,
            CliPostAction::Snapshot => Self::Snapshot,
            CliPostAction::Screenshot => Self::Screenshot,
            CliPostAction::Annotated => Self::Annotated,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliReadTextMode {
    Value,
    Selection,
    Clipboard,
}

impl From<CliReadTextMode> for ReadTextMode {
    fn from(v: CliReadTextMode) -> Self {
        match v {
            CliReadTextMode::Value => Self::Value,
            CliReadTextMode::Selection => Self::Selection,
            CliReadTextMode::Clipboard => Self::Clipboard,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPolicyMode {
    Open,
    Confirm,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket_path);
    let fleet = cli.fleet.unwrap_or_else(default_fleet_path);
    let host = Some(cli.host.as_str());

    if matches!(cli.command, Command::Mcp) {
        return desktop::mcp::serve_stdio(socket, fleet).await;
    }

    let client = DesktopClient::new_with_fleet(socket.clone(), fleet.clone(), "cli");
    let host_owned = cli.host.clone();
    let is_remote = host_owned != "local" && !host_owned.is_empty();
    let result = run_cli_command(&client, cli.json, cli.command, &socket, &fleet, host).await;
    if is_remote {
        let _ = client.disconnect_remote(&host_owned).await;
    }
    result
}

async fn run_cli_command(
    client: &DesktopClient,
    json_out: bool,
    command: Command,
    socket: &std::path::Path,
    fleet: &std::path::Path,
    host: Option<&str>,
) -> Result<()> {
    match command {
        Command::Mcp => unreachable!(),
        Command::Hosts => {
            let hosts = client.list_hosts().await;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&hosts)?);
            } else {
                for h in hosts {
                    if let Some(st) = h.status {
                        println!(
                            "{:<10} ONLINE  ({:>3} ms)  {:<22}  {:<13}  ax={} sc={}  lease={}",
                            h.alias,
                            h.latency_ms,
                            st.host,
                            format!("{:?}", st.screen_state),
                            st.permissions.accessibility,
                            st.permissions.screen_recording,
                            st.active_lease
                                .as_ref()
                                .map_or("none", |l| l.owner.as_str())
                        );
                    } else {
                        println!(
                            "{:<10} OFFLINE ({:>3} ms)  {}  ({})",
                            h.alias,
                            h.latency_ms,
                            h.addr,
                            h.error.as_deref().unwrap_or("unreachable")
                        );
                    }
                }
            }
        }
        Command::Fleet {
            action: FleetAction::Init { port, force },
        } => {
            init_fleet(fleet, port, force)?;
        }
        Command::Status => {
            let res = client.call(host, RpcMethod::Status).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if let RpcResult::Status(st) = res {
                println!("host: {} ({}) · desktopd {}", st.host, st.os, st.version);
                println!("screen: {:?} · displays: {}", st.screen_state, st.displays);
                println!(
                    "permissions: accessibility={}, screen_recording={}",
                    st.permissions.accessibility, st.permissions.screen_recording
                );
                println!("policy: {:?}", st.policy_mode);
                if let Some(lease) = st.active_lease {
                    println!("active lease: {} ({})", lease.owner, lease.session_id);
                } else {
                    println!("active lease: none");
                }
            }
        }
        Command::Windows { app } => {
            let res = client
                .call(
                    host,
                    RpcMethod::ListWindows(ListWindowsParams {
                        app,
                        include_displays: true,
                    }),
                )
                .await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if let RpcResult::Windows { displays, windows } = res {
                for d in displays {
                    println!(
                        "display {} (id={}{}) {:.0}x{:.0} pts ({}x{} px, scale {:.1}x)",
                        d.index,
                        d.id,
                        if d.main { ", main" } else { "" },
                        d.bounds.width,
                        d.bounds.height,
                        d.pixel_width,
                        d.pixel_height,
                        d.scale
                    );
                }
                for w in windows {
                    println!(
                        "w{} [{}] {:?} ({:.0},{:.0} {:.0}x{:.0}){}",
                        w.id,
                        w.app.name,
                        w.title,
                        w.bounds.x,
                        w.bounds.y,
                        w.bounds.width,
                        w.bounds.height,
                        if w.focused { " *focused*" } else { "" }
                    );
                }
            }
        }
        Command::Snapshot {
            app,
            window,
            root,
            max_depth,
            max_nodes,
            focus,
            bounds,
        } => {
            let res = client
                .call(
                    host,
                    RpcMethod::Snapshot(SnapshotParams {
                        app,
                        window_id: window,
                        root_ref: root,
                        max_depth,
                        max_nodes,
                        focus_mode: focus.into(),
                        include_bounds: bounds,
                    }),
                )
                .await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if let RpcResult::Snapshot(snap) = res {
                print!(
                    "{}",
                    snap.render_compact_with_options(SnapshotRenderOptions {
                        include_bounds: bounds,
                    })
                );
            }
        }
        Command::Find {
            query,
            role,
            app,
            window,
            limit,
        } => {
            let res = client
                .call(
                    host,
                    RpcMethod::Find(FindParams {
                        query,
                        role,
                        app,
                        window_id: window,
                        limit,
                    }),
                )
                .await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if let RpcResult::Find {
                snapshot_id,
                matches,
            } = res
            {
                println!("snapshot_id: {snapshot_id} · {} match(es)", matches.len());
                for m in matches {
                    println!(
                        "  {} {} {:?} path={}",
                        m.ref_id,
                        m.role,
                        m.title.or(m.label).unwrap_or_default(),
                        m.path.join(" > ")
                    );
                }
            }
        }
        Command::Screenshot {
            display,
            window,
            app,
            annotate,
            format,
            out,
            focus,
        } => {
            let target = if let Some(window_id) = window {
                ScreenshotTarget::Window { window_id }
            } else if let Some(app) = app {
                ScreenshotTarget::App { app }
            } else {
                ScreenshotTarget::Display { index: display }
            };
            let res = client
                .call(
                    host,
                    RpcMethod::Screenshot(ScreenshotParams {
                        target,
                        annotate,
                        format: format.into(),
                        if_changed_since: None,
                        focus_mode: focus.into(),
                    }),
                )
                .await?;
            if let RpcResult::Screenshot(shot) = res {
                if let Some(path) = out {
                    let b64 = shot.data_base64.as_deref().unwrap_or("");
                    let bytes = BASE64.decode(b64).context("decode screenshot base64")?;
                    std::fs::write(&path, &bytes)
                        .with_context(|| format!("write {}", path.display()))?;
                    println!(
                        "saved {} ({}x{}, {} bytes, hash={})",
                        path.display(),
                        shot.width,
                        shot.height,
                        bytes.len(),
                        shot.hash
                    );
                } else if json_out {
                    println!("{}", serde_json::to_string_pretty(&shot)?);
                } else {
                    println!(
                        "captured {}x{} ({}, hash={}, annotated={})",
                        shot.width,
                        shot.height,
                        shot.mime_type,
                        shot.hash,
                        shot.annotated_refs.len()
                    );
                }
            }
        }
        Command::Act {
            steps_json,
            after,
            focus,
            release_after,
        } => {
            let steps: Vec<ActionStep> =
                serde_json::from_str(&steps_json).context("parse steps JSON array")?;
            let res = client
                .call(
                    host,
                    RpcMethod::Act(ActParams {
                        steps,
                        after: after.into(),
                        expect_snapshot: None,
                        focus_mode: focus.into(),
                        release_after,
                    }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
        Command::ReadText { ref_id, mode } => {
            let res = client
                .call(
                    host,
                    RpcMethod::ReadText(ReadTextParams {
                        ref_id,
                        window_id: None,
                        mode: mode.into(),
                    }),
                )
                .await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if let RpcResult::ReadText(rt) = res {
                println!("{}", rt.text);
            }
        }
        Command::Menu { app, path } => {
            let res = client
                .call(host, RpcMethod::Menu(MenuParams { app, path }))
                .await?;
            if let RpcResult::Menu { detail } = res {
                println!("{detail}");
            }
        }
        Command::Stop => {
            let res = client.call(host, RpcMethod::Stop).await?;
            if let RpcResult::Stopped { had_active_session } = res {
                println!(
                    "{}",
                    if had_active_session {
                        "Stopped active desktop session."
                    } else {
                        "No active desktop session."
                    }
                );
            }
        }
        Command::Policy { action } => match action.unwrap_or(PolicyAction::Show) {
            PolicyAction::Show => {
                let res = client.call(host, RpcMethod::PolicyGet).await?;
                println!("{}", serde_json::to_string_pretty(&res)?);
            }
            PolicyAction::SetMode { mode } => {
                let mut cfg = match client.call(host, RpcMethod::PolicyGet).await? {
                    RpcResult::Policy(c) => c,
                    _ => unreachable!(),
                };
                cfg.mode = match mode {
                    CliPolicyMode::Open => PolicyMode::Open,
                    CliPolicyMode::Confirm => PolicyMode::Confirm,
                };
                let res = client.call(host, RpcMethod::PolicySet(cfg)).await?;
                println!("{}", serde_json::to_string_pretty(&res)?);
            }
        },
        Command::Grant => {
            let res = client.call(host, RpcMethod::RequestPermissions).await?;
            if let RpcResult::Status(st) = res {
                if st.os == "macos" {
                    println!(
                        "Requested macOS TCC registration for AgentDesktop.app (accessibility={}, screen_recording={}).",
                        st.permissions.accessibility, st.permissions.screen_recording
                    );
                    println!(
                        "Toggle AgentDesktop on in System Settings > Privacy & Security, then restart:"
                    );
                    println!("  launchctl kickstart -k \"gui/$(id -u)/com.syz.agent-desktop\"");
                } else {
                    println!(
                        "{} does not require macOS TCC grants (accessibility={}, screen_recording={}).",
                        st.os, st.permissions.accessibility, st.permissions.screen_recording
                    );
                }
            }
        }
        Command::Doctor => {
            println!("socket: {}", socket.display());
            match client.call(host, RpcMethod::Status).await {
                Ok(RpcResult::Status(st)) => {
                    println!(
                        "daemon: reachable (desktopd {}, host={})",
                        st.version, st.host
                    );
                    println!(
                        "accessibility: {}",
                        if st.permissions.accessibility {
                            "OK"
                        } else {
                            "MISSING"
                        }
                    );
                    println!(
                        "screen_recording: {}",
                        if st.permissions.screen_recording {
                            "OK"
                        } else {
                            "MISSING"
                        }
                    );
                    println!(
                        "screen_state: {:?} (displays={})",
                        st.screen_state, st.displays
                    );
                    println!("policy_mode: {:?}", st.policy_mode);
                }
                Ok(_) => {}
                Err(err) => {
                    println!("daemon: NOT REACHABLE ({err})");
                }
            }
            #[cfg(target_os = "macos")]
            {
                let app_path = std::path::Path::new("/Applications/AgentDesktop.app");
                if app_path.exists() {
                    let sig = std::process::Command::new("codesign")
                        .args(["-dv", "--verbose=2", "/Applications/AgentDesktop.app"])
                        .output();
                    match sig {
                        Ok(out) => {
                            let info = String::from_utf8_lossy(&out.stderr);
                            let auth = info
                                .lines()
                                .find(|l| {
                                    l.starts_with("Authority=") || l.starts_with("Signature=")
                                })
                                .unwrap_or("unknown signature");
                            println!("bundle: /Applications/AgentDesktop.app ({auth})");
                        }
                        Err(e) => println!("bundle: codesign check failed ({e})"),
                    }
                } else {
                    println!(
                        "bundle: /Applications/AgentDesktop.app not installed yet (run ./packaging/macos/build-app.sh)"
                    );
                }
            }
            #[cfg(target_os = "linux")]
            {
                let dpy = std::env::var("DISPLAY").unwrap_or_else(|_| "(unset)".into());
                let wl = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "(unset)".into());
                let session =
                    std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into());
                println!("linux_session: type={session} DISPLAY={dpy} WAYLAND_DISPLAY={wl}");
                let tools: Vec<&str> = [
                    "Xvfb", "xclip", "xsel", "grim", "wl-copy", "ydotool", "wtype",
                ]
                .into_iter()
                .filter(|bin| {
                    std::env::var_os("PATH")
                        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                })
                .collect();
                println!(
                    "linux_tools: {}",
                    if tools.is_empty() {
                        "none detected".into()
                    } else {
                        tools.join(", ")
                    }
                );
            }
        }
        Command::Bench { iterations, app } => {
            run_bench(client, host, iterations.clamp(1, 200), app, json_out).await?;
        }
        Command::Profile {
            app,
            query,
            screenshot,
        } => {
            let report =
                desktop::profile::run_live_profile(client, host, app, query, screenshot).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", desktop::profile::render_report_table(&report));
            }
        }
    }

    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct BenchRow {
    op: &'static str,
    iterations: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    note: String,
}

fn summarize_samples(op: &'static str, mut samples: Vec<f64>, note: String) -> BenchRow {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len().max(1);
    let min_ms = samples.first().copied().unwrap_or(0.0);
    let max_ms = samples.last().copied().unwrap_or(0.0);
    let p50_ms = samples[(n - 1) / 2];
    let p95_idx = ((n as f64) * 0.95).ceil() as usize;
    let p95_ms = samples[p95_idx.saturating_sub(1).min(n - 1)];
    let mean_ms = samples.iter().sum::<f64>() / (n as f64);
    BenchRow {
        op,
        iterations: samples.len(),
        min_ms,
        p50_ms,
        p95_ms,
        max_ms,
        mean_ms,
        note,
    }
}

async fn run_bench(
    client: &DesktopClient,
    host: Option<&str>,
    iterations: usize,
    app: Option<String>,
    json_out: bool,
) -> Result<()> {
    use std::time::Instant;

    let mut rows = Vec::new();

    // 1. status (RPC transport floor)
    let mut s_status = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        let _ = client.call(host, RpcMethod::Status).await?;
        s_status.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples("status", s_status, "RPC floor".into()));

    // 2. list_windows
    let mut s_win = Vec::with_capacity(iterations);
    let mut last_win_count = 0usize;
    for _ in 0..iterations {
        let t = Instant::now();
        if let RpcResult::Windows { windows, .. } = client
            .call(
                host,
                RpcMethod::ListWindows(ListWindowsParams {
                    app: app.clone(),
                    include_displays: true,
                }),
            )
            .await?
        {
            last_win_count = windows.len();
        }
        s_win.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples(
        "list_windows",
        s_win,
        format!("{last_win_count} windows"),
    ));

    // 3. snapshot (AX tree)
    let mut s_snap = Vec::with_capacity(iterations);
    let mut last_nodes = 0usize;
    for _ in 0..iterations {
        let t = Instant::now();
        if let RpcResult::Snapshot(snap) = client
            .call(
                host,
                RpcMethod::Snapshot(SnapshotParams {
                    app: app.clone(),
                    focus_mode: FocusMode::Background,
                    ..SnapshotParams::default()
                }),
            )
            .await?
        {
            last_nodes = snap.emitted_nodes;
        }
        s_snap.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples(
        "snapshot",
        s_snap,
        format!("{last_nodes} nodes"),
    ));

    // 4. screenshot (warm / stream + ring buffer)
    // Prime warm SCStream once, then measure warm display screenshots.
    let first_shot = client
        .call(
            host,
            RpcMethod::Screenshot(ScreenshotParams {
                target: ScreenshotTarget::Display { index: None },
                annotate: false,
                format: ImageFormat::Jpeg,
                if_changed_since: None,
                focus_mode: FocusMode::Background,
            }),
        )
        .await?;
    let mut last_hash = String::new();
    let mut last_dims = String::new();
    if let RpcResult::Screenshot(s) = first_shot {
        last_hash = s.hash;
        last_dims = format!("{}x{}", s.width, s.height);
    }

    // Wait briefly so warm SCStream delivers a live frame, then benchmark rapid display captures.
    tokio::time::sleep(std::time::Duration::from_millis(140)).await;
    let mut s_shot = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        let _ = client
            .call(
                host,
                RpcMethod::Screenshot(ScreenshotParams {
                    target: ScreenshotTarget::Display { index: None },
                    annotate: false,
                    format: ImageFormat::Jpeg,
                    if_changed_since: None,
                    focus_mode: FocusMode::Background,
                }),
            )
            .await?;
        s_shot.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples(
        "screenshot (warm)",
        s_shot,
        last_dims.clone(),
    ));

    // 5. screenshot (unchanged poll via if_changed_since)
    let mut s_unchanged = Vec::with_capacity(iterations);
    let mut unchanged_hits = 0usize;
    for _ in 0..iterations {
        let t = Instant::now();
        if let RpcResult::Screenshot(s) = client
            .call(
                host,
                RpcMethod::Screenshot(ScreenshotParams {
                    target: ScreenshotTarget::Display { index: None },
                    annotate: false,
                    format: ImageFormat::Jpeg,
                    if_changed_since: Some(last_hash.clone()),
                    focus_mode: FocusMode::Background,
                }),
            )
            .await?
        {
            if s.unchanged {
                unchanged_hits += 1;
            }
            last_hash = s.hash;
        }
        s_unchanged.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples(
        "screenshot (poll)",
        s_unchanged,
        format!("{unchanged_hits}/{iterations} unchanged"),
    ));

    // 6. screenshot (annotated SoM)
    let ann_iters = iterations.min(5);
    let mut s_ann = Vec::with_capacity(ann_iters);
    for _ in 0..ann_iters {
        let t = Instant::now();
        let _ = client
            .call(
                host,
                RpcMethod::Screenshot(ScreenshotParams {
                    target: ScreenshotTarget::Display { index: None },
                    annotate: true,
                    format: ImageFormat::Jpeg,
                    if_changed_since: None,
                    focus_mode: FocusMode::Background,
                }),
            )
            .await?;
        s_ann.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(summarize_samples(
        "screenshot (SoM)",
        s_ann,
        format!("{last_dims} annotated"),
    ));

    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!(
            "agent-desktop benchmark (host={}, iterations={})\n",
            host.unwrap_or("local"),
            iterations
        );
        println!(
            "{:<20} {:>8} {:>8} {:>8} {:>8}  Notes",
            "Operation", "min ms", "p50 ms", "p95 ms", "max ms"
        );
        println!("{}", "-".repeat(72));
        for r in &rows {
            println!(
                "{:<20} {:>8.1} {:>8.1} {:>8.1} {:>8.1}  {}",
                r.op, r.min_ms, r.p50_ms, r.p95_ms, r.max_ms, r.note
            );
        }
    }
    Ok(())
}

fn init_fleet(fleet_path: &std::path::Path, port: u16, force: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = fleet_path
        .parent()
        .context("fleet.toml path has no parent directory")?;
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let token_path = dir.join("token");
    if !token_path.exists() || force {
        let bytes: [u8; 32] = rand::rng().random();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(&token_path, format!("{hex}\n"))?;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;
        println!("Generated 0600 bearer token at {}", token_path.display());
    } else {
        println!("Keeping existing bearer token at {}", token_path.display());
    }

    if !fleet_path.exists() || force {
        let body = format!(
            "# agent-desktop Tailscale fleet config (private, 0600 — never commit to public repos)\n\
             [listen]\n\
             bind = \"tailscale\"\n\
             port = {port}\n\
             token_file = \"{}\"\n\
             require_tailscale = true\n\
             \n\
             # Add peer machines below (copy the same 0600 token file to each peer):\n\
             # [hosts.pro]\n\
             # addr = \"100.x.y.z:{port}\"\n\
             # token_file = \"{}\"\n",
            token_path.display(),
            token_path.display()
        );
        std::fs::write(fleet_path, body)?;
        std::fs::set_permissions(fleet_path, std::fs::Permissions::from_mode(0o600))?;
        println!("Wrote 0600 fleet config to {}", fleet_path.display());
    } else {
        println!(
            "Keeping existing fleet config at {} (use --force to overwrite)",
            fleet_path.display()
        );
    }

    println!("\nNext steps:");
    println!("  1. Restart desktopd so it binds its Tailscale IP:");
    #[cfg(target_os = "macos")]
    println!("     launchctl kickstart -k \"gui/$(id -u)/com.syz.agent-desktop\"");
    #[cfg(target_os = "linux")]
    println!("     systemctl --user restart desktopd   # or desktopd-xvfb");
    println!(
        "  2. Copy {} (0600) to your peer machines and add [hosts.<name>] entries.",
        token_path.display()
    );
    println!("  3. Run `desktop hosts` to verify connectivity.");
    Ok(())
}
