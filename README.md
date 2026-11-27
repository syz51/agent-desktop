# agent-desktop

Fast, owner-first Rust computer-use daemon, CLI, and MCP server for controlling macOS and Linux desktops locally, headlessly (`Xvfb`), and across your Tailscale fleet.

## Architecture

- **`desktopd`** (`crates/desktop-daemon`): Persistent per-machine daemon packaged inside `AgentDesktop.app` on macOS (for stable Accessibility & Screen Recording TCC grants) or run as a `systemd --user` service on Linux. Listens on a `0600` Unix domain socket locally and optionally on your Tailscale IP (`:9487`) with constant-time 256-bit bearer auth.
- **`desktop`** (`crates/desktop`): Thin, fast client binary with two interfaces:
  - `desktop mcp`: Stdio MCP server (`rmcp`) exposing 12 lean tools (`list_hosts`, `session_start`, `session_end`, `list_windows`, `snapshot`, `find`, `screenshot`, `act`, `read_text`, `menu`, `wait`, `status`).
  - `desktop <command>`: CLI for humans and scripts (`hosts`, `fleet init`, `status`, `windows`, `snapshot`, `find`, `screenshot`, `act`, `read-text`, `menu`, `stop`, `policy`, `grant`, `doctor`).
- **`desktop-proto`** (`crates/desktop-proto`): Wire types, validation, coordinate mapping, X11/macOS key tables, fleet config, error codes (`#![forbid(unsafe_code)]`).
- **`desktop-core`** (`crates/desktop-core`): Session lease, opt-in policy engine (`PolicyMode::Open` default), batch action runner, AX tree pruner/searcher, shared Set-of-Marks RGBA annotator, JSONL audit log, injectable `Clock`, `PlatformBackend` trait (`#![forbid(unsafe_code)]`).
- **`desktop-macos`** (`crates/desktop-macos`): Native macOS backend (`AXUIElement`, `ScreenCaptureKit` with `desktopIndependentWindow` background capture, `CGEvent` + clipboard paste, `IOKit` display wake/keep-awake, `AppKit` floating Stop/Esc overlay).
- **`desktop-linux`** (`crates/desktop-linux`): Pure-Rust `#![forbid(unsafe_code)]` Linux backend:
  - **X11 & Managed Headless `Xvfb`**: `x11rb` RandR + EWMH windows + direct/composited window & display ZPixmap capture + `XTest` input + `--xvfb` 24/7 virtual display manager.
  - **AT-SPI2 (`zbus`)**: Semantic accessibility tree snapshots with role normalization, zero-focus background `Action.DoAction`, `EditableText.SetTextContents`, `Text.GetText`, and graceful window-level fallback when AT-SPI is absent.
  - **Wayland**: `grim` (wlroots) or `org.freedesktop.portal.Screenshot` (`zbus`) capture, `wl-clipboard`, and `ydotool`/`wtype` input support.

## Quick Start (macOS)

```sh
# 1. Create a persistent local self-signed Code Signing cert (once per Mac)
./packaging/macos/setup-signing.sh

# 2. Build, sign, install /Applications/AgentDesktop.app, and load LaunchAgent
./packaging/macos/build-app.sh

# 3. Trigger macOS TCC registration if needed, then verify
desktop grant
desktop doctor
```

## Quick Start (Linux / Headless Server)

```sh
# Interactive X11 / Wayland desktop (installs ~/.local/bin/{desktop,desktopd} + desktopd.service)
./packaging/linux/install-linux.sh

# Or 24/7 headless Linux server/VPS with managed private Xvfb display (requires `xvfb`)
./packaging/linux/install-linux.sh --xvfb

# Verify daemon health and detected session/tools
desktop doctor
```

## Tailscale Fleet Setup

```sh
# Initialize 0600 bearer token + starter ~/.config/agent-desktop/fleet.toml
desktop fleet init

# Copy ~/.config/agent-desktop/token (0600) to peer machines, add [hosts.<alias>] entries,
# restart desktopd, then check live reachability & latency across all machines:
desktop hosts
desktop --host <alias> windows
```

## Development Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
shellcheck packaging/macos/*.sh packaging/linux/*.sh
```
