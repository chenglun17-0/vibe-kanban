#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE_DIR="$PROJECT_ROOT/tmp/vibe-kanban-dev"

echo "=== Stopping Vibe Kanban development services ==="

pid_cwd() {
  local pid="$1"

  if [[ -e "/proc/$pid/cwd" ]]; then
    readlink "/proc/$pid/cwd" 2>/dev/null || true
  elif command -v lsof >/dev/null 2>&1; then
    lsof -a -p "$pid" -d cwd -Fn 2>/dev/null |
      awk '/^n/ { sub(/^n/, ""); print; exit }'
  fi
}

is_project_pid() {
  local pid="$1"
  local cwd
  cwd="$(pid_cwd "$pid")"
  [[ "$cwd" == "$PROJECT_ROOT" || "$cwd" == "$PROJECT_ROOT/"* ]]
}

is_self_or_ancestor() {
  local candidate="$1"
  local current="$$"

  while [[ "$current" -gt 1 ]]; do
    if [[ "$current" == "$candidate" ]]; then
      return 0
    fi
    current="$(ps -o ppid= -p "$current" 2>/dev/null | tr -d ' ')"
    [[ -n "$current" ]] || break
  done

  return 1
}

TREE_PIDS=()
collect_process_tree() {
  local pid="$1"
  local child

  while read -r child; do
    [[ -n "$child" ]] || continue
    collect_process_tree "$child"
  done < <(pgrep -P "$pid" 2>/dev/null || true)

  TREE_PIDS+=("$pid")
}

wait_for_exit() {
  local attempts="$1"
  shift
  local pid

  for ((i = 0; i < attempts; i++)); do
    local running=false
    for pid in "$@"; do
      if kill -0 "$pid" 2>/dev/null; then
        running=true
        break
      fi
    done
    [[ "$running" == false ]] && return 0
    sleep 0.5
  done

  return 1
}

terminate_tree() {
  local label="$1"
  local root_pid="$2"

  kill -0 "$root_pid" 2>/dev/null || return 0
  TREE_PIDS=()
  collect_process_tree "$root_pid"

  echo "[$label] Terminating process tree rooted at PID $root_pid..."
  kill -TERM "${TREE_PIDS[@]}" 2>/dev/null || true
  if ! wait_for_exit 20 "${TREE_PIDS[@]}"; then
    echo "[$label] Force killing remaining processes..."
    local pid
    for pid in "${TREE_PIDS[@]}"; do
      kill -KILL "$pid" 2>/dev/null || true
    done
  fi
}

graceful_backend_shutdown() {
  local root_pid="$1"
  local server_pids=()
  local pid command

  TREE_PIDS=()
  collect_process_tree "$root_pid"
  for pid in "${TREE_PIDS[@]}"; do
    command="$(ps -o command= -p "$pid" 2>/dev/null || true)"
    if [[ "$command" == *"target/debug/server"* || "$command" == *"target/release/server"* ]]; then
      server_pids+=("$pid")
    fi
  done

  if [[ "${#server_pids[@]}" -gt 0 ]]; then
    echo "[backend] Requesting graceful shutdown for PID(s): ${server_pids[*]}..."
    kill -TERM "${server_pids[@]}" 2>/dev/null || true
    wait_for_exit 20 "${server_pids[@]}" || true
  fi
}

stop_from_pid_file() {
  local label="$1"
  local file_name="$2"
  local graceful="${3:-false}"
  local pid_file="$STATE_DIR/$file_name"

  if [[ ! -f "$pid_file" ]]; then
    echo "[$label] No PID file found; checking for legacy processes later."
    return
  fi

  local pid
  pid="$(cat "$pid_file")"
  if [[ ! "$pid" =~ ^[0-9]+$ ]] || ! kill -0 "$pid" 2>/dev/null; then
    echo "[$label] Recorded process is no longer running."
    rm -f "$pid_file"
    return
  fi

  if ! is_project_pid "$pid"; then
    echo "[$label] Refusing to stop PID $pid because it is outside this repository." >&2
    rm -f "$pid_file"
    return
  fi

  if [[ "$graceful" == true ]]; then
    graceful_backend_shutdown "$pid"
  fi
  terminate_tree "$label" "$pid"
  rm -f "$pid_file"
}

matches_dev_command() {
  local command="$1"
  [[ "$command" == *"pnpm run dev"* ||
    "$command" == *"pnpm run backend:dev:watch"* ||
    "$command" == *"pnpm run frontend:dev"* ||
    "$command" == *"concurrently"*"backend:dev:watch"* ||
    "$command" == *"cargo-watch"*"run --bin server"* ||
    "$command" == *"target/debug/server"* ||
    "$command" == *"target/release/server"* ||
    "$command" == *"vite"*"--port"* ]]
}

find_legacy_processes() {
  local pid command
  ps -axo pid=,command= | while read -r pid command; do
    [[ -n "$pid" ]] || continue
    matches_dev_command "$command" || continue
    is_self_or_ancestor "$pid" && continue
    is_project_pid "$pid" || continue
    printf '%s\n' "$pid"
  done
}

stop_legacy_processes() {
  local legacy_pids=()
  local pid command

  while read -r pid; do
    [[ -n "$pid" ]] && legacy_pids+=("$pid")
  done < <(find_legacy_processes)

  [[ "${#legacy_pids[@]}" -gt 0 ]] || return 0
  echo "[cleanup] Found legacy development processes: ${legacy_pids[*]}"

  local server_pids=()
  for pid in "${legacy_pids[@]}"; do
    command="$(ps -o command= -p "$pid" 2>/dev/null || true)"
    if [[ "$command" == *"target/debug/server"* || "$command" == *"target/release/server"* ]]; then
      server_pids+=("$pid")
    fi
  done
  if [[ "${#server_pids[@]}" -gt 0 ]]; then
    echo "[backend] Requesting graceful shutdown for legacy PID(s): ${server_pids[*]}..."
    kill -TERM "${server_pids[@]}" 2>/dev/null || true
    wait_for_exit 20 "${server_pids[@]}" || true
  fi

  while read -r pid; do
    [[ -n "$pid" ]] || continue
    terminate_tree "cleanup" "$pid"
  done < <(find_legacy_processes)
}

stop_from_pid_file "backend" "backend.pid" true
stop_from_pid_file "frontend" "frontend.pid"
stop_legacy_processes

rm -f "$STATE_DIR/backend.port" "$STATE_DIR/frontend.port"

echo
echo "=== Development services stopped ==="
echo "  Logs retained in: $STATE_DIR/log"
