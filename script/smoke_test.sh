#!/usr/bin/env bash
# Smoke test: start hls2dash, wait for MPD, exercise enable/disable, stop.
#
# Requires a reachable HLS playlist URL via SMOKE_HLS_URL.
# Do not hard-code real production origins in this script.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${SMOKE_HLS_URL:-}" ]]; then
  echo "error: set SMOKE_HLS_URL to a reachable HLS playlist URL" >&2
  echo "  example: SMOKE_HLS_URL='http://origin.example.com/live/stream/index.m3u8' $0" >&2
  exit 1
fi

PORT="${SMOKE_PORT:-18080}"
CHANNEL="${SMOKE_CHANNEL:-demo}"
CFG="${ROOT}/script/.smoke/config.yaml"
mkdir -p "${ROOT}/script/.smoke"

cat >"$CFG" <<EOF
dash:
  listen: "127.0.0.1"
  port: ${PORT}
cache:
  dir: "${ROOT}/script/.smoke/cache"
  segment_duration_secs: 2
  window_segments: 30
  cleanup_interval_secs: 60
reconnect_secs: 3
pull:
  - url: "${SMOKE_HLS_URL}"
    channel: "${CHANNEL}"
    enable: true
EOF

cleanup() {
  CONFIG="$CFG" "${ROOT}/script/stop.sh" >/dev/null 2>&1 || true
  # also kill by port if needed
  if command -v lsof >/dev/null 2>&1; then
    lsof -ti tcp:"$PORT" | xargs kill -9 2>/dev/null || true
  fi
}
trap cleanup EXIT

rm -rf "${ROOT}/script/.smoke/cache"
CONFIG="$CFG" "${ROOT}/script/start.sh"

echo "Waiting for MPD..."
ok=false
for i in $(seq 1 60); do
  if curl -fsS --max-time 2 "http://127.0.0.1:${PORT}/live/${CHANNEL}/index.mpd" >/dev/null 2>&1; then
    ok=true
    break
  fi
  sleep 1
done
if [[ "$ok" != true ]]; then
  echo "FAIL: MPD not ready" >&2
  tail -n 100 "${ROOT}/script/hls2dash.log" >&2 || true
  exit 1
fi
echo "OK: MPD available"

curl -fsS "http://127.0.0.1:${PORT}/healthz" | grep -q ok
curl -fsS "http://127.0.0.1:${PORT}/channels" | grep -q "${CHANNEL}"
curl -fsS "http://127.0.0.1:${PORT}/api/channels" | grep -q "${CHANNEL}"

echo "Testing disable..."
curl -fsS -X POST "http://127.0.0.1:${PORT}/api/channels/${CHANNEL}/disable" >/dev/null
sleep 1
# after disable, discovery list should eventually not show as running
# (channel still in /api/channels with enabled=false)
curl -fsS "http://127.0.0.1:${PORT}/api/channels" | grep -q '"enabled":false\|"enabled": false' || \
  python3 -c "import json,urllib.request; d=json.load(urllib.request.urlopen('http://127.0.0.1:${PORT}/api/channels')); assert any(c['id']=='${CHANNEL}' and not c['enabled'] for c in d)"

echo "Testing enable..."
curl -fsS -X POST "http://127.0.0.1:${PORT}/api/channels/${CHANNEL}/enable" >/dev/null
sleep 2

echo "smoke_test OK"
