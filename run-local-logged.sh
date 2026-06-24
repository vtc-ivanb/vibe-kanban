#!/usr/bin/env bash
#
# Run the locally-built "node" (npx CLI) version of vibe-kanban with the file
# log sink enabled, writing to ./logs/ in the repo root.
#
# This launches the binary that the npx CLI extracts from npx-cli/dist/, i.e.
# the release `server` binary produced by ./local-build.sh — so it includes the
# diff-stream instrumentation and the file log sink from this branch. (Plain
# `npx vibe-kanban` downloads the *published* binary, which does NOT have them.)
#
# Prerequisites (run once after changing Rust code):
#   ./local-build.sh                 # builds npx-cli/dist/<platform>/vibe-kanban.zip
#
# Usage:
#   ./run-local-logged.sh                # browser mode (headless server)
#   ./run-local-logged.sh --desktop      # desktop app (needs ./local-build.sh --desktop)
#   VK_LOG_FILTER="trace" ./run-local-logged.sh   # widen what the file captures
#
# The log filter for the FILE sink is independent of RUST_LOG (which only
# affects the terminal). By default the file captures info everywhere, debug for
# services/server/local_deployment, and trace for the diff stream + filesystem
# watcher — see utils::logging::DEFAULT_FILE_FILTER. Override with VK_LOG_FILTER.

set -euo pipefail

# Repo root = this script's directory.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- Ensure the npx CLI entrypoint is built ---------------------------------
CLI_JS="$ROOT/npx-cli/bin/cli.js"
if [ ! -f "$CLI_JS" ]; then
  echo "==> npx-cli/bin/cli.js not found; building it..."
  ( cd "$ROOT/npx-cli" && { [ -d node_modules ] || npm install; } && npm run build )
fi

# --- Ensure a locally-built binary exists -----------------------------------
if [ ! -d "$ROOT/npx-cli/dist" ]; then
  echo "error: npx-cli/dist/ not found." >&2
  echo "       Build the local binary first:  ./local-build.sh" >&2
  exit 1
fi

# --- Prepare the log file ----------------------------------------------------
LOG_DIR="$ROOT/logs"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/vibe-kanban-$(date +%Y%m%d-%H%M%S).log"

# A native Windows binary needs a Windows path (E:\...), not the MSYS /e/... form.
if command -v cygpath >/dev/null 2>&1; then
  export VK_LOG_FILE="$(cygpath -w "$LOG_FILE")"
else
  export VK_LOG_FILE="$LOG_FILE"
fi

# Force the CLI to use the locally-built binary from npx-cli/dist/.
export VIBE_KANBAN_LOCAL=1

echo "==> Logging to: $VK_LOG_FILE"
echo "==> Filter:     ${VK_LOG_FILTER:-<default: diff_stream/filesystem_watcher=trace>}"
echo "==> Launching local vibe-kanban (npx CLI)..."

exec node "$CLI_JS" "$@"
