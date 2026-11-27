# Contributing to agent-desktop

Thanks for helping. The project is in early development, so the design still moves; open an
issue before you start on anything larger than a fix, so we can agree on the approach first.

## Set up

You need:

- Rust 1.99 or later (`rustup` installs the stable toolchain the workspace builds with);
- `shellcheck` for checking the packaging scripts;
- On Linux for live X11/Xvfb tests: `xvfb` and `xclip` (e.g. `sudo apt install xvfb xclip`).

```sh
git clone https://github.com/syz51/agent-desktop
cd agent-desktop
cargo test --workspace
```

`cargo run -p desktop -- --help` and `cargo run -p desktop-daemon -- --help` list the
commands and daemon flags.

## Before you push

Run the checks in the README's [Development](README.md#development) section. CI runs the
same checks across macOS and Linux (including live `xvfb-run` tests) on every pull request.

## Conventions

[`AGENTS.md`](AGENTS.md) holds the project rules, for people and coding agents alike. The
ones that matter most in review:

- read [`docs/DESIGN.md`](docs/DESIGN.md) before changing behavior;
- keep `unsafe` strictly inside `crates/desktop-macos` with `// SAFETY:` comments; every
  other crate enforces `#![forbid(unsafe_code)]`;
- add tests with every behavior change;
- this repository is public: examples, tests and fixtures use placeholders, never real
  Tailscale IPs, hostnames, bearer tokens, private paths or personal screenshots.

## Proposing a change

1. For a bug or feature, find or open an [issue](https://github.com/syz51/agent-desktop/issues/new/choose).
   Report security problems privately instead, as [`SECURITY.md`](SECURITY.md) explains.
2. Fork, branch from `main`, and keep each pull request to one change.
3. Open the pull request and fill in the template. CI (`fmt`, `macos`, `linux`) must pass
   before it merges; review conversations must be resolved.

By contributing you agree that your work is released under the [MIT license](LICENSE), and
to follow the [code of conduct](CODE_OF_CONDUCT.md).
