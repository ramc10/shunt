#!/bin/sh
set -e

REPO="ramc10/shunt"
BIN="shunt"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS and arch
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
      arm64)   TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      arm64)  TARGET="aarch64-apple-darwin" ;;
      x86_64) echo "Intel Mac binaries are not provided. Install Rust and run: cargo install shunt-proxy" >&2; exit 1 ;;
      *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *) echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

# Fetch latest release version via redirect (avoids JSON parsing and CDN caching)
echo "Fetching latest release..."
LATEST_URL="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
VERSION="$(echo "$LATEST_URL" | sed 's|.*/tag/||')"

if [ -z "$VERSION" ]; then
  echo "Could not determine latest version." >&2
  exit 1
fi

ARCHIVE="shunt-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$ARCHIVE"

# Download and extract
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $BIN $VERSION for $TARGET..."
curl -fsSL "$URL" -o "$TMP/$ARCHIVE"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP"

# Install binary
mkdir -p "$INSTALL_DIR"
cp "$TMP/shunt-${VERSION}-${TARGET}/$BIN" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

# macOS: remove quarantine and ad-hoc sign so Gatekeeper allows unsigned binaries
if [ "$OS" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$INSTALL_DIR/$BIN" 2>/dev/null || true
  codesign --force --deep --sign - "$INSTALL_DIR/$BIN" 2>/dev/null || true
fi

echo ""
echo "Installed $BIN $VERSION to $INSTALL_DIR/$BIN"

# Ensure INSTALL_DIR is on PATH for this session so the binary runs immediately
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) export PATH="$INSTALL_DIR:$PATH" ;;
esac

EXE="$INSTALL_DIR/$BIN"

# ── Write login-service file (no launchctl/systemctl — avoids SSH hangs) ─────
if [ "$OS" = "Darwin" ]; then
  PLIST_DIR="$HOME/Library/LaunchAgents"
  mkdir -p "$PLIST_DIR"
  cat > "$PLIST_DIR/sh.shunt.proxy.plist" << ENDPLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>sh.shunt.proxy</string>
  <key>ProgramArguments</key>
  <array>
    <string>$EXE</string>
    <string>start</string>
    <string>--foreground</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$HOME/Library/Logs/shunt.log</string>
  <key>StandardErrorPath</key>
  <string>$HOME/Library/Logs/shunt.log</string>
</dict>
</plist>
ENDPLIST
elif [ "$OS" = "Linux" ]; then
  UNIT_DIR="$HOME/.config/systemd/user"
  mkdir -p "$UNIT_DIR"
  cat > "$UNIT_DIR/shunt.service" << ENDUNIT
[Unit]
Description=shunt Claude Code proxy
After=network.target

[Service]
ExecStart=$EXE start --foreground
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
ENDUNIT
fi

# ── Start proxy now ───────────────────────────────────────────────────────────
echo "Starting shunt..."
"$EXE" start < /dev/null

# ── Write ANTHROPIC_BASE_URL to shell profile (belt-and-suspenders) ───────────
for PROFILE in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile"; do
  if [ -f "$PROFILE" ]; then
    if ! grep -q "ANTHROPIC_BASE_URL" "$PROFILE" 2>/dev/null; then
      printf '\n# Added by shunt\nexport ANTHROPIC_BASE_URL=http://127.0.0.1:8082\n' >> "$PROFILE"
    fi
    break
  fi
done
