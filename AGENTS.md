# Working in agent-desktop

- Read `docs/DESIGN.md` before changing architecture or RPC behavior. `crates/desktop-proto`
  is the wire contract shared by `desktopd`, `desktop` CLI/MCP, and remote Tailscale peers:
  keep changes backward compatible within `/v1` and update its tests together.
- This repository is public. Never commit real Tailscale IPs, public IPs, machine hostnames,
  bearer tokens, private file paths, or screenshots of personal windows; examples and tests
  use placeholders.
- Crate boundaries:
  - `desktop-proto`: wire types, validation, coordinate mapping, key tables, fleet config
    (`#![forbid(unsafe_code)]`).
  - `desktop-core`: session lease, opt-in policy engine, batch action runner, AX tree search,
    shared Set-of-Marks RGBA annotator, JSONL audit log, injectable `Clock`, `PlatformBackend`
    trait (`#![forbid(unsafe_code)]`).
  - `desktop-macos`: native macOS backend (`AXUIElement`, `ScreenCaptureKit`, `CGEvent`,
    `IOKit`, `AppKit`). The only crate permitted to use `unsafe`, with `#![deny(unsafe_op_in_unsafe_fn)]`
    and `// SAFETY:` comments on every unsafe block.
  - `desktop-linux`: pure-Rust Linux backend (`x11rb` X11/RandR/XTest, authenticated `Xvfb`,
    AT-SPI2 over `zbus`, Wayland portal/tools) (`#![forbid(unsafe_code)]`).
  - `desktop-daemon`: `desktopd` daemon (0600 UDS + optional Tailscale HTTP RPC).
  - `desktop`: thin CLI and stdio MCP server (`rmcp`).
- Add a crate's dependencies in its own `Cargo.toml`; only shared dependencies belong in the
  workspace manifest.
- Validate inputs at the RPC/CLI boundary; pass typed values inward. Errors returned to
  agents should say what to do next (`RpcError::with_next`).
- Time comes from the injectable `Clock` trait so lease expiry and staleness are testable.
- Checks before every commit:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `shellcheck packaging/macos/*.sh packaging/linux/*.sh`
- Add tests with every behavior change.

## Merging

- `main` takes changes only through a PR whose `fmt`, `macos`, and `linux` checks pass on a
  branch that is up to date with `main`, with every review thread resolved. Keep that rule.
- When a PR is ready, run `gh pr merge <N> --merge --auto`; GitHub merges it once the checks
  pass. When it is behind `main`, run `gh pr update-branch <N>` too.
- Merge `main` locally only when `gh pr update-branch` reports a conflict; resolve it, run
  the checks above, and push.
