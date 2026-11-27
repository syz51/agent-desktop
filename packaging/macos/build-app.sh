#!/usr/bin/env bash
set -euo pipefail

# Builds release binaries (`desktopd` + `desktop`), packages /Applications/AgentDesktop.app,
# signs it with the persistent local Code Signing certificate (so macOS TCC grants persist),
# installs `~/.local/bin/desktop`, and loads the gui LaunchAgent.

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP_BUNDLE="${AGENT_DESKTOP_APP:-/Applications/AgentDesktop.app}"
BUNDLE_ID="${AGENT_DESKTOP_BUNDLE_ID:-com.syz.agent-desktop}"
CERT_NAME="${AGENT_DESKTOP_CERT_NAME:-AgentDesktop Local Signer}"
KEYCHAIN="${AGENT_DESKTOP_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
BIN_DIR="${AGENT_DESKTOP_BIN_DIR:-$HOME/.local/bin}"
LAUNCH_AGENT_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="$LAUNCH_AGENT_DIR/$BUNDLE_ID.plist"

if ! security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -Fq "\"$CERT_NAME\""; then
    echo "ERROR: Local signing certificate '$CERT_NAME' not found." >&2
    echo "Run ./packaging/macos/setup-signing.sh once first so TCC permissions survive rebuilds." >&2
    exit 1
fi

CERT_SHA1="$(
    security find-identity -v -p codesigning "$KEYCHAIN" \
        | grep -F "\"$CERT_NAME\"" \
        | head -n 1 \
        | awk '{print $2}'
)"

echo "Building release workspace..."
cargo build --manifest-path "$REPO_ROOT/Cargo.toml" --release --bin desktopd --bin desktop

STAGE_APP="$(mktemp -d "${TMPDIR:-/tmp}/AgentDesktop.XXXXXX")/AgentDesktop.app"
trap 'rm -rf "$(dirname "$STAGE_APP")"' EXIT

mkdir -p "$STAGE_APP/Contents/MacOS"
cp "$REPO_ROOT/target/release/desktopd" "$STAGE_APP/Contents/MacOS/desktopd"
chmod 755 "$STAGE_APP/Contents/MacOS/desktopd"

sed "s|__BUNDLE_ID__|$BUNDLE_ID|g" "$REPO_ROOT/packaging/macos/Info.plist.tmpl" \
    > "$STAGE_APP/Contents/Info.plist"

# Pin the designated requirement to our bundle ID + local certificate leaf hash so tccd
# recognizes every future rebuild as the exact same app identity.
REQ="designated => identifier \"$BUNDLE_ID\" and certificate leaf = H\"$CERT_SHA1\""

echo "Signing $STAGE_APP with '$CERT_NAME' ($CERT_SHA1)..."
codesign --force --sign "$CERT_SHA1" --requirements "=$REQ" "$STAGE_APP/Contents/MacOS/desktopd"
codesign --force --sign "$CERT_SHA1" --requirements "=$REQ" "$STAGE_APP"
codesign --verify --deep --strict --verbose=2 "$STAGE_APP"

echo "Installing $APP_BUNDLE..."
rm -rf "$APP_BUNDLE"
cp -R "$STAGE_APP" "$APP_BUNDLE"

mkdir -p "$BIN_DIR"
# Atomic write + rename to a new inode so Apple Silicon macOS never hits a stale
# vnode code-signature cache (`SIGKILL` / "killed") when replacing a previously run binary.
TMP_BIN="$BIN_DIR/desktop.tmp.$$"
cp "$REPO_ROOT/target/release/desktop" "$TMP_BIN"
chmod 755 "$TMP_BIN"
codesign --force --sign - "$TMP_BIN"
mv -f "$TMP_BIN" "$BIN_DIR/desktop"
echo "Installed CLI/MCP binary to $BIN_DIR/desktop"

mkdir -p "$LAUNCH_AGENT_DIR" "$HOME/.local/state/agent-desktop"
sed \
    -e "s|__BUNDLE_ID__|$BUNDLE_ID|g" \
    -e "s|__APP_BIN__|$APP_BUNDLE/Contents/MacOS/desktopd|g" \
    -e "s|__LOG_DIR__|$HOME/.local/state/agent-desktop|g" \
    "$REPO_ROOT/packaging/macos/com.syz.agent-desktop.plist.tmpl" \
    > "$PLIST_PATH"

UID_NUM="$(id -u)"
if launchctl print "gui/$UID_NUM/$BUNDLE_ID" >/dev/null 2>&1; then
    launchctl bootout "gui/$UID_NUM/$BUNDLE_ID" 2>/dev/null || true
    for _ in $(seq 1 30); do
        launchctl print "gui/$UID_NUM/$BUNDLE_ID" >/dev/null 2>&1 || break
        sleep 0.1
    done
fi
launchctl bootstrap "gui/$UID_NUM" "$PLIST_PATH"
launchctl kickstart -k "gui/$UID_NUM/$BUNDLE_ID"

echo "AgentDesktop.app installed and desktopd LaunchAgent started."
echo "Run '$BIN_DIR/desktop doctor' to verify TCC permissions and daemon status."
