#!/usr/bin/env bash
# Start hls2dash in the background (release build).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PID_FILE="${ROOT}/script/hls2dash.pid"
LOG_FILE="${ROOT}/script/hls2dash.log"
CONFIG="${CONFIG:-${ROOT}/config.yaml}"

if [[ ! -f "$CONFIG" ]]; then
  echo "error: config not found: $CONFIG" >&2
  echo "  copy the example first: cp config.example.yaml config.yaml" >&2
  exit 1
fi

resolve_release_bin() {
  local name="${1:-hls2dash}"
  local target_dir=""
  if command -v cargo >/dev/null 2>&1; then
    target_dir="$(
      cargo metadata --format-version=1 --no-deps --manifest-path "${ROOT}/Cargo.toml" 2>/dev/null \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null \
        || true
    )"
  fi
  if [[ -z "$target_dir" ]]; then
    target_dir="${CARGO_TARGET_DIR:-${ROOT}/target}"
  fi
  echo "${target_dir}/release/${name}"
}

resolve_bin() {
  if [[ -x "${ROOT}/bin/hls2dash" ]]; then
    echo "${ROOT}/bin/hls2dash"
    return
  fi
  resolve_release_bin hls2dash
}

read_dash_port() {
  local dash_port=8080
  if [[ -f "$CONFIG" ]] && command -v python3 >/dev/null 2>&1; then
    dash_port="$(
      python3 - "$CONFIG" <<'PY'
import sys
path = sys.argv[1]
dash = 8080
try:
    import yaml
    with open(path) as f:
        doc = yaml.safe_load(f) or {}
    dash = int(((doc.get("dash") or {}).get("port")) or 8080)
except Exception:
    text = open(path).read().splitlines()
    section = None
    for line in text:
        s = line.strip()
        if s.startswith("dash:"):
            section = "dash"
            continue
        if s and not s.startswith("#") and not line.startswith(" ") and not line.startswith("\t") and s.endswith(":"):
            section = None
            continue
        if section == "dash" and s.startswith("port:"):
            try:
                dash = int(s.split(":", 1)[1].strip().strip('"').strip("'"))
            except Exception:
                pass
print(dash)
PY
    )"
  fi
  echo "$dash_port"
}

http_ready() {
  local port="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 0.5 "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1 && return 0
  fi
  return 1
}

BIN="$(resolve_bin)"
DASH_PORT="$(read_dash_port)"

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  if [[ -n "${old_pid}" ]] && kill -0 "$old_pid" 2>/dev/null; then
    echo "hls2dash already running (pid=${old_pid})"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

if [[ "$BIN" != "${ROOT}/bin/hls2dash" ]]; then
  echo "Building release binary..."
  cargo build --release --manifest-path "${ROOT}/Cargo.toml"
  BIN="$(resolve_bin)"
fi

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found: $BIN" >&2
  exit 1
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "error: config not found: $CONFIG" >&2
  exit 1
fi

mkdir -p "${ROOT}/cache" "$(dirname "$LOG_FILE")"
: >"$LOG_FILE"
echo "Starting hls2dash (config=${CONFIG}, bin=${BIN}, dash=${DASH_PORT})..."
nohup "$BIN" --config "$CONFIG" >>"$LOG_FILE" 2>&1 &
pid=$!
echo "$pid" >"$PID_FILE"

ready=false
for _ in $(seq 1 50); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "error: process exited during startup" >&2
    tail -n 80 "$LOG_FILE" >&2 || true
    rm -f "$PID_FILE"
    exit 1
  fi
  if http_ready "$DASH_PORT"; then
    ready=true
    break
  fi
  sleep 0.2
done

if [[ "$ready" != true ]]; then
  echo "error: HTTP :${DASH_PORT}/healthz not ready after 10s" >&2
  tail -n 80 "$LOG_FILE" >&2 || true
  kill "$pid" 2>/dev/null || true
  rm -f "$PID_FILE"
  exit 1
fi

echo "hls2dash started (pid=${pid})"
echo "  log: $LOG_FILE"
echo "  play: http://<host>:${DASH_PORT}/live/<channel>/index.mpd"
