#!/usr/bin/env bash
set -euo pipefail

# Build and install desktop + desktopd on Linux, and optionally enable a systemd --user unit.
# Usage:
#   ./packaging/linux/install-linux.sh           # interactive X11/Wayland session unit
#   ./packaging/linux/install-linux.sh --xvfb    # 24/7 headless Xvfb unit

MODE="interactive"
if [[ "${1:-}" == "--xvfb" ]]; then
  MODE="xvfb"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_DIR="${HOME}/.config/systemd/user"

echo "Building release workspace..."
cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" --release --bin desktop --bin desktopd

mkdir -p "${BIN_DIR}"

install_atomic() {
  local src="$1"
  local dst="$2"
  local tmp="${dst}.new.$$"
  cp "${src}" "${tmp}"
  chmod 0755 "${tmp}"
  mv -f "${tmp}" "${dst}"
}

install_atomic "${REPO_ROOT}/target/release/desktop" "${BIN_DIR}/desktop"
install_atomic "${REPO_ROOT}/target/release/desktopd" "${BIN_DIR}/desktopd"
echo "Installed binaries to ${BIN_DIR}/desktop and ${BIN_DIR}/desktopd"

if command -v systemctl >/dev/null 2>&1; then
  mkdir -p "${SYSTEMD_DIR}"
  cp "${REPO_ROOT}/packaging/linux/desktopd.service" "${SYSTEMD_DIR}/desktopd.service"
  cp "${REPO_ROOT}/packaging/linux/desktopd-xvfb.service" "${SYSTEMD_DIR}/desktopd-xvfb.service"

  UNIT="desktopd.service"
  if [[ "${MODE}" == "xvfb" ]]; then
    UNIT="desktopd-xvfb.service"
  fi

  if systemctl --user daemon-reload >/dev/null 2>&1; then
    systemctl --user enable --now "${UNIT}"
    echo "Enabled and started ${UNIT}"
  else
    echo "Installed systemd user units to ${SYSTEMD_DIR} (no active user bus to reload right now)."
  fi
fi

echo "Run '${BIN_DIR}/desktop doctor' to verify daemon status and Linux session capabilities."
