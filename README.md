# agent-desktop

Fast, owner-first Rust computer-use daemon, CLI, and MCP server for controlling macOS (and planned Linux) desktops locally and across your Tailscale fleet.

## Architecture

- **`desktopd`** (`crates/desktop-daemon`): Persistent per-machine daemon packaged inside `AgentDesktop.app` on macOS (for stable Accessibility & Screen Recording TCC grants). Runs over a `0600` Unix domain socket locally.
- **`desktop`** (`crates/desktop`): Thin, fast client binary with two interfaces:
  - `desktop mcp`: Stdio MCP server (`rmcp`) exposing 12 lean tools (`list_hosts`, `session_start`, `session_end`, `list_windows`, `snapshot`, `find`, `screenshot`, `act`, `read_text`, `menu`, `wait`, `status`).
  - `desktop <command>`: CLI for humans and scripts (`status`, `windows`, `snapshot`, `find`, `screenshot`, `act`, `read-text`, `menu`, `wait`, `stop`, `policy`, `doctor`).
- **`desktop-proto`** (`crates/desktop-proto`): Wire types, validation, coordinate mapping, error codes (`#![forbid(unsafe_code)]`).
- **`desktop-core`** (`crates/desktop-core`): Session lease, opt-in policy engine, batch action runner, AX tree pruner/searcher, JSONL audit log, injectable `Clock`, `PlatformBackend` trait (`#![forbid(unsafe_code)]`).
- **`desktop-macos`** (`crates/desktop-macos`): Native macOS backend (`AXUIElement`, `ScreenCaptureKit` with `desktopIndependentWindow` background capture + Set-of-Marks annotation, `CGEvent` + clipboard paste, `IOKit` display wake/keep-awake, `AppKit` floating Stop/Esc overlay).

## Quick Start (macOS)

```sh
# 1. Create a persistent local self-signed Code Signing cert (once per Mac)
./packaging/macos/setup-signing.sh

# 2. Build, sign, install /Applications/AgentDesktop.app, and load LaunchAgent
./packaging/macos/build-app.sh

# 3. Verify permissions and live daemon health
cargo run -p desktop -- doctor
```

## Development Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
