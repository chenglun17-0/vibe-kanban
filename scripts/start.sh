#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE_DIR="$PROJECT_ROOT/tmp/vibe-kanban-dev"
LOG_DIR="$STATE_DIR/log"
mkdir -p "$STATE_DIR" "$LOG_DIR"

echo "=== Starting Vibe Kanban development services ==="

echo "[cleanup] Stopping existing development services..."
"$PROJECT_ROOT/scripts/stop.sh"

if ! command -v pnpm >/dev/null 2>&1; then
  echo "pnpm is required. Install dependencies with: pnpm install" >&2
  exit 1
fi

if [[ -z "${FRONTEND_PORT:-}" ]]; then
  FRONTEND_PORT="$(node "$PROJECT_ROOT/scripts/setup-dev-environment.js" frontend)"
fi
if [[ -z "${BACKEND_PORT:-}" ]]; then
  BACKEND_PORT="$(node "$PROJECT_ROOT/scripts/setup-dev-environment.js" backend)"
fi
VK_ALLOWED_ORIGINS="${VK_ALLOWED_ORIGINS:-http://localhost:${FRONTEND_PORT}}"

printf '%s\n' "$FRONTEND_PORT" >"$STATE_DIR/frontend.port"
printf '%s\n' "$BACKEND_PORT" >"$STATE_DIR/backend.port"

start_service() {
  local label="$1"
  local pid_file="$2"
  local log_file="$3"
  shift 3

  echo "[$label] Starting..."
  (
    cd "$PROJECT_ROOT"
    nohup "$@" >"$log_file" 2>&1 &
    printf '%s\n' "$!" >"$pid_file"
  )

  local pid
  pid="$(cat "$pid_file")"
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "[$label] Failed to start. Last log lines:" >&2
    tail -n 20 "$log_file" >&2 || true
    "$PROJECT_ROOT/scripts/stop.sh"
    exit 1
  fi

  echo "[$label] PID: $pid (log: ${log_file#"$PROJECT_ROOT/"})"
}

start_service \
  "backend" \
  "$STATE_DIR/backend.pid" \
  "$LOG_DIR/backend.log" \
  env BACKEND_PORT="$BACKEND_PORT" VK_ALLOWED_ORIGINS="$VK_ALLOWED_ORIGINS" \
  DISABLE_WORKTREE_CLEANUP=1 RUST_LOG="${RUST_LOG:-debug}" \
  pnpm run backend:dev:watch

start_service \
  "frontend" \
  "$STATE_DIR/frontend.pid" \
  "$LOG_DIR/frontend.log" \
  env FRONTEND_PORT="$FRONTEND_PORT" BACKEND_PORT="$BACKEND_PORT" \
  VK_ALLOWED_ORIGINS="$VK_ALLOWED_ORIGINS" \
  pnpm run frontend:dev

echo
echo "=== Development services started ==="
echo "  Frontend: http://localhost:${FRONTEND_PORT}"
echo "  Backend:  http://localhost:${BACKEND_PORT}"
echo "  Logs:     $LOG_DIR"
echo "  Stop:     $PROJECT_ROOT/scripts/stop.sh"
