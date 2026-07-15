#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PID_FILE="${ROOT}/script/hls2dash.pid"

if [[ ! -f "$PID_FILE" ]]; then
  echo "hls2dash is not running (no pid file)"
  exit 0
fi
pid="$(cat "$PID_FILE" 2>/dev/null || true)"
if [[ -n "${pid}" ]] && kill -0 "$pid" 2>/dev/null; then
  kill "$pid" || true
  for _ in $(seq 1 30); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null || true
  fi
  echo "hls2dash stopped (pid=${pid})"
else
  echo "hls2dash not running (stale pid file)"
fi
rm -f "$PID_FILE"
