# agent-desktop

[![CI](https://github.com/syz51/agent-desktop/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/syz51/agent-desktop/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Fast, owner-first Rust computer-use daemon, CLI, and MCP server for controlling macOS and
Linux desktops locally, headlessly (`Xvfb`), and across a private Tailscale fleet.

- **Accessibility-first, vision-ready**: inspects native UI trees (`AXUIElement` on macOS,
  AT-SPI2 on Linux) with compact `@eN` element refs, plus in-memory Set-of-Marks screenshots
  when visual grounding is needed.
- **Background-capable**: `focus_mode = "auto" | "background" | "foreground"` uses
  `SCContentFilter(desktopIndependentWindow:)` and semantic actions without stealing focus
  when supported, and 100% isolated headless `Xvfb` sessions on Linux servers.
- **Owner-first policy**: runs unrestricted (`PolicyMode::Open`) by default with zero modal
  nagging, while preserving a physical `Esc` kill-switch, `desktop stop`, JSONL audit trail,
  and opt-in `deny_apps`, `confirm_apps`, and `redact_passwords` in `policy.toml`.
- **Private Tailscale fleet**: routes CLI and MCP calls seamlessly across your own Macs and
  Linux hosts (`desktop --host <alias> ...`) over `100.64.0.0/10` with constant-time 256-bit
  bearer auth and automatic lease release on disconnect.

Status: early development. Architecture and protocol design live in
[`docs/DESIGN.md`](docs/DESIGN.md).

## Architecture

| Crate / Binary | Role |
|---|---|
| `desktopd` (`crates/desktop-daemon`) | Persistent per-machine daemon (`0600` local UDS + optional Tailscale HTTP RPC) |
| `desktop` (`crates/desktop`) | Thin CLI + 12-tool stdio MCP server (`desktop mcp` via `rmcp`) |
| `desktop-proto` (`crates/desktop-proto`) | Wire types, validation, coordinate mapping, macOS/X11 key tables, fleet config |
| `desktop-core` (`crates/desktop-core`) | Session leases, opt-in policy engine, batch action runner, tree search, Set-of-Marks annotator, audit log |
| `desktop-macos` (`crates/desktop-macos`) | Native macOS backend (`AXUIElement`, `ScreenCaptureKit`, `CGEvent`, `IOKit`, `AppKit` Stop pill) |
| `desktop-linux` (`crates/desktop-linux`) | Pure-Rust (`#![forbid(unsafe_code)]`) Linux backend (`x11rb` X11/RandR/XTest, authenticated `Xvfb`, AT-SPI2 via `zbus`, Wayland portal/`grim`) |

## Install

### Homebrew (macOS and Linux)

```sh
brew tap syz51/tap
brew trust --formula syz51/tap/agent-desktop
brew install syz51/tap/agent-desktop
```

> Omit `brew trust` on older Homebrew versions that do not provide it. On macOS, if you want
> persistent TCC permissions across upgrades, use the signed app bundle flow in Quick Start below.

### Prebuilt Release Archives

Tagged [releases](https://github.com/syz51/agent-desktop/releases) ship `desktop` and
`desktopd` archives for macOS (Apple silicon and Intel) and Linux (`x86_64` and `aarch64`)
with a `SHA256SUMS` file and GitHub build provenance attestations:

```sh
shasum -a 256 --check --ignore-missing SHA256SUMS
gh release verify-asset v<version> agent-desktop-<version>-<target>.tar.gz --repo syz51/agent-desktop
gh attestation verify agent-desktop-<version>-<target>.tar.gz --repo syz51/agent-desktop   # optional
tar -xzf agent-desktop-<version>-<target>.tar.gz
```

## Quick Start

### macOS (from source, with stable TCC signing)

```sh
# 1. Create a persistent local self-signed Code Signing cert (once per Mac)
./packaging/macos/setup-signing.sh

# 2. Build, sign, install /Applications/AgentDesktop.app, and start the LaunchAgent
./packaging/macos/build-app.sh

# 3. Register Accessibility & Screen Recording permissions if prompted, then verify
desktop grant
desktop doctor
```

### Linux / Headless Server

```sh
# Interactive X11 / Wayland desktop (installs ~/.local/bin/{desktop,desktopd} + desktopd.service)
./packaging/linux/install-linux.sh

# Or 24/7 headless Linux server/VPS with private authenticated Xvfb display (requires `xvfb`)
./packaging/linux/install-linux.sh --xvfb

# Verify daemon health and detected session capabilities
desktop doctor
```

### Tailscale Fleet

```sh
# Initialize a 0600 bearer token and starter ~/.config/agent-desktop/fleet.toml
desktop fleet init

# Copy ~/.config/agent-desktop/token (0600) to peer machines, add [hosts.<alias>] entries,
# restart desktopd, then inspect reachability and latency across your fleet:
desktop hosts
desktop --host <alias> windows
```

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
shellcheck packaging/macos/*.sh packaging/linux/*.sh
```

## Contributing

Issues and pull requests are welcome; [`CONTRIBUTING.md`](CONTRIBUTING.md) explains how.
Report vulnerabilities privately, as [`SECURITY.md`](SECURITY.md) describes.

## License

[MIT](LICENSE)
