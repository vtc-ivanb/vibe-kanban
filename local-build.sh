#!/bin/bash

set -e  # Exit on any error

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Map architecture names
case "$ARCH" in
  x86_64)
    ARCH="x64"
    ;;
  arm64|aarch64)
    ARCH="arm64"
    ;;
  *)
    echo "⚠️  Warning: Unknown architecture $ARCH, using as-is"
    ;;
esac

# Map OS names. Windows builds run under Git Bash / MSYS2, where `uname -s`
# reports MINGW64_NT-*, MSYS_NT-*, or CYGWIN_NT-*.
case "$OS" in
  linux)
    OS="linux"
    ;;
  darwin)
    OS="macos"
    ;;
  mingw*|msys*|cygwin*)
    OS="windows"
    ;;
  *)
    echo "⚠️  Warning: Unknown OS $OS, using as-is"
    ;;
esac

PLATFORM="${OS}-${ARCH}"

# Windows executables carry a .exe suffix; other platforms have none.
EXE=""
if [ "$OS" = "windows" ]; then
  EXE=".exe"
fi

# Set CARGO_TARGET_DIR if not defined
if [ -z "$CARGO_TARGET_DIR" ]; then
  CARGO_TARGET_DIR="target"
fi

echo "🔍 Detected platform: $PLATFORM"
echo "🔧 Using target directory: $CARGO_TARGET_DIR"

# On Windows, bindgen (via libsqlite3-sys) needs libclang. Auto-detect a default
# LLVM install if LIBCLANG_PATH isn't already set.
if [ "$OS" = "windows" ] && [ -z "$LIBCLANG_PATH" ]; then
  if [ -f "/c/Program Files/LLVM/bin/libclang.dll" ]; then
    export LIBCLANG_PATH="/c/Program Files/LLVM/bin"
    echo "🔧 Auto-set LIBCLANG_PATH=$LIBCLANG_PATH"
  else
    echo "⚠️  LIBCLANG_PATH not set and LLVM not found at C:\\Program Files\\LLVM."
    echo "    The Rust build may fail in libsqlite3-sys/bindgen."
    echo "    Install LLVM with: winget install LLVM.LLVM"
  fi
fi

# Portable zip helper: prefer `zip`, fall back to 7-Zip, then PowerShell.
# Usage: make_zip <output.zip> <file>
make_zip() {
  local out="$1" file="$2"
  rm -f "$out"
  if command -v zip >/dev/null 2>&1; then
    zip -q "$out" "$file"
  elif command -v 7z >/dev/null 2>&1; then
    7z a -tzip -bso0 -bsp0 "$out" "$file" >/dev/null
  elif [ -x "/c/Program Files/7-Zip/7z.exe" ]; then
    "/c/Program Files/7-Zip/7z.exe" a -tzip -bso0 -bsp0 "$out" "$file" >/dev/null
  elif command -v powershell.exe >/dev/null 2>&1; then
    powershell.exe -NoProfile -Command "Compress-Archive -Path '$file' -DestinationPath '$out' -Force"
  else
    echo "❌ No zip tool found (need 'zip', '7z', or PowerShell)" >&2
    exit 1
  fi
}

# Set API base URL for remote features
export VK_SHARED_API_BASE="https://api.vibekanban.com"
export VITE_VK_SHARED_API_BASE="https://api.vibekanban.com"

echo "🧹 Cleaning previous builds..."
rm -rf npx-cli/dist
mkdir -p npx-cli/dist/$PLATFORM

echo "🔨 Building web app..."
(cd packages/local-web && npm run build)

echo "🔨 Building Rust binaries..."
cargo build --release --manifest-path Cargo.toml
cargo build --release --bin vibe-kanban-mcp --manifest-path Cargo.toml

echo "📦 Creating distribution package..."

# Copy the main binary (renamed to vibe-kanban; the npm CLI extracts
# vibe-kanban${EXE} from the archive, so the suffix must be preserved).
cp "${CARGO_TARGET_DIR}/release/server${EXE}" "vibe-kanban${EXE}"
make_zip vibe-kanban.zip "vibe-kanban${EXE}"
rm -f "vibe-kanban${EXE}"
mv vibe-kanban.zip npx-cli/dist/$PLATFORM/vibe-kanban.zip

# Copy the MCP binary
cp "${CARGO_TARGET_DIR}/release/vibe-kanban-mcp${EXE}" "vibe-kanban-mcp${EXE}"
make_zip vibe-kanban-mcp.zip "vibe-kanban-mcp${EXE}"
rm -f "vibe-kanban-mcp${EXE}"
mv vibe-kanban-mcp.zip npx-cli/dist/$PLATFORM/vibe-kanban-mcp.zip

# Copy the Review CLI binary
cp "${CARGO_TARGET_DIR}/release/review${EXE}" "vibe-kanban-review${EXE}"
make_zip vibe-kanban-review.zip "vibe-kanban-review${EXE}"
rm -f "vibe-kanban-review${EXE}"
mv vibe-kanban-review.zip npx-cli/dist/$PLATFORM/vibe-kanban-review.zip

echo "✅ CLI build complete!"
echo "📁 Files created:"
echo "   - npx-cli/dist/$PLATFORM/vibe-kanban.zip"
echo "   - npx-cli/dist/$PLATFORM/vibe-kanban-mcp.zip"
echo "   - npx-cli/dist/$PLATFORM/vibe-kanban-review.zip"

# Optionally build the Tauri desktop app
if [[ "$1" == "--desktop" || "$1" == "--all" ]]; then
  # Map to Tauri platform naming
  case "$OS" in
    macos) TAURI_OS="darwin" ;;
    linux) TAURI_OS="linux" ;;
    windows) TAURI_OS="windows" ;;
    *) TAURI_OS="$OS" ;;
  esac
  case "$ARCH" in
    arm64) TAURI_ARCH="aarch64" ;;
    x64) TAURI_ARCH="x86_64" ;;
    *) TAURI_ARCH="$ARCH" ;;
  esac
  TAURI_PLATFORM="${TAURI_OS}-${TAURI_ARCH}"

  echo ""
  echo "🖥️  Building Tauri desktop app for $TAURI_PLATFORM..."

  # Replace the updater endpoint placeholder with a dummy URL for local builds
  # (CI injects the real R2 URL; locally the updater is non-functional)
  TAURI_CONF="crates/tauri-app/tauri.conf.json"
  node -e "
    const fs = require('fs');
    const conf = JSON.parse(fs.readFileSync('$TAURI_CONF', 'utf8'));
    conf.plugins.updater.endpoints = conf.plugins.updater.endpoints.map(e =>
      e === '__TAURI_UPDATE_ENDPOINT__' ? 'https://localhost/disabled' : e
    );
    fs.writeFileSync('$TAURI_CONF', JSON.stringify(conf, null, 2) + '\n');
  "

  cargo tauri build

  # Restore tauri.conf.json
  git checkout -- "$TAURI_CONF"

  TAURI_DIST="npx-cli/dist/tauri/$TAURI_PLATFORM"
  mkdir -p "$TAURI_DIST"

  BUNDLE_DIR="${CARGO_TARGET_DIR}/release/bundle"
  # Copy updater artifacts (tar.gz bundles or NSIS exe)
  find "$BUNDLE_DIR" -name "*.app.tar.gz" ! -name "*.sig" -exec cp {} "$TAURI_DIST/" \; 2>/dev/null || true
  find "$BUNDLE_DIR" -name "*.AppImage.tar.gz" ! -name "*.sig" -exec cp {} "$TAURI_DIST/" \; 2>/dev/null || true
  find "$BUNDLE_DIR" -name "*-setup.exe" -exec cp {} "$TAURI_DIST/" \; 2>/dev/null || true

  echo "✅ Desktop app built:"
  ls -la "$TAURI_DIST/"
fi

echo ""
echo "📦 Installing npx-cli dependencies..."
(cd npx-cli && npm ci)

echo ""
echo "🔨 Building npx-cli TypeScript..."
(cd npx-cli && npm run build)

echo ""
echo "🚀 To test locally, run:"
echo "   cd npx-cli && node bin/cli.js                # browser mode (default)"
echo "   cd npx-cli && node bin/cli.js --desktop       # desktop mode (requires --desktop or --all build flag)"
