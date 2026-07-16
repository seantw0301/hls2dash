#!/usr/bin/env bash
# Interactive VLC review for a numeric HLS range.
# Each kept channel is APPENDED immediately to config_pre.yaml (never overwrite).
# Runtime still uses config.yaml — copy from config_pre.yaml when ready.
#
# Example:
#   BASE_URL=http://origin.example.com/cc ./script/review_channels.sh 20 50
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_CONFIG="${ROOT}/config_pre.yaml"
SELECTIONS_FILE="${ROOT}/script/.review_selections.tsv"

usage() {
  cat <<EOF
Usage: BASE_URL=<hls-base> $(basename "$0") [-o config_pre.yaml] START END

  Interactively play HLS streams sh_START .. sh_END in VLC.
  Each kept channel is APPENDED immediately to config_pre.yaml
  (existing pull: entries are never overwritten).

  Example:
    BASE_URL=http://origin.example.com/cc $(basename "$0") 20 50
    BASE_URL=http://origin.example.com/cc $(basename "$0") 100 120

  Prompt: y = keep (then enter channel id), n = skip

Options:
  -o FILE   Config path to append into (default: ${ROOT}/config_pre.yaml)
  -h        Show help

Env:
  BASE_URL  Required. HLS base prefix (trailing / stripped).
            URL pattern: \${BASE_URL}/sh_XXX/index.m3u8
EOF
}

if [[ -z "${BASE_URL:-}" ]]; then
  echo "error: BASE_URL is required (do not hard-code real origins in this script)" >&2
  usage
  exit 1
fi

# Avoid …/cc//sh_001 when BASE_URL already ends with /
BASE_URL="${BASE_URL%/}"

while getopts ":o:h" opt; do
  case "$opt" in
    o) OUT_CONFIG="$OPTARG" ;;
    h) usage; exit 0 ;;
    \?) echo "unknown option: -$OPTARG" >&2; usage; exit 1 ;;
  esac
done
shift $((OPTIND - 1))

if [[ $# -ne 2 ]]; then
  echo "error: need START END (e.g. 20 50)" >&2
  usage
  exit 1
fi

START="$1"
END="$2"

if ! [[ "$START" =~ ^[0-9]+$ && "$END" =~ ^[0-9]+$ ]]; then
  echo "error: START and END must be integers" >&2
  exit 1
fi
if (( START > END )); then
  echo "error: START ($START) must be <= END ($END)" >&2
  exit 1
fi
if (( START < 0 || END > 999 )); then
  echo "error: range must be within 0..999 (sh_000..sh_999)" >&2
  exit 1
fi

find_vlc() {
  if command -v vlc >/dev/null 2>&1; then
    echo "vlc"
    return
  fi
  if [[ -x "/Applications/VLC.app/Contents/MacOS/VLC" ]]; then
    echo "/Applications/VLC.app/Contents/MacOS/VLC"
    return
  fi
  return 1
}

if ! VLC_BIN="$(find_vlc)"; then
  echo "error: VLC not found (install VLC or put 'vlc' on PATH)" >&2
  exit 1
fi

STREAMS=()
for i in $(seq "$START" "$END"); do
  STREAMS+=("$(printf 'sh_%03d' "$i")")
done

mkdir -p "$(dirname "$SELECTIONS_FILE")" "$(dirname "$OUT_CONFIG")"
: >"$SELECTIONS_FILE"

EXISTING_IDS_FILE="$(mktemp)"
EXISTING_URLS_FILE="$(mktemp)"
trap 'rm -f "$EXISTING_IDS_FILE" "$EXISTING_URLS_FILE"' EXIT

# Create config_pre.yaml skeleton only when missing. Never truncate/overwrite.
create_config_base_if_missing() {
  if [[ -f "$OUT_CONFIG" ]]; then
    if grep -qE '^pull:' "$OUT_CONFIG"; then
      return 0
    fi
    echo "error: $OUT_CONFIG has no top-level pull: section to append into" >&2
    exit 1
  fi
  cat >"$OUT_CONFIG" <<'HDR'
# Egress mode: dash | mpegts (exactly one).
# dash   → http://host:port/live/<channel>/index.mpd
# mpegts → http://host:port/live/<channel>/mpegts
#   (direct HLS .ts stitch → continuous TS; no CMAF)
output_mode: dash

dash:
  listen: "0.0.0.0"
  port: 8080

cache:
  dir: "./cache"
  segment_duration_secs: 2
  window_segments: 90
  cleanup_interval_secs: 180

# Used when output_mode: mpegts (trans_server-aligned knobs).
mpegts:
  live_holdback_segments: 6
  min_buffer_segments: 3
  max_segment_lag: 10
  send_queue: 32
  poll_interval_secs: 2
  pace_egress: true
  ingest_poll_factor: 0.25

# Global: fail → wait N seconds → retry (all channels).
reconnect_secs: 3

pull:
HDR
  echo "Created new config (first run): $OUT_CONFIG"
}

normalize_pull_header() {
  if grep -qE '^pull:[[:space:]]*\[\][[:space:]]*$' "$OUT_CONFIG"; then
    if sed --version >/dev/null 2>&1; then
      sed -i 's/^pull:[[:space:]]*\[\][[:space:]]*$/pull:/' "$OUT_CONFIG"
    else
      sed -i '' 's/^pull:[[:space:]]*\[\][[:space:]]*$/pull:/' "$OUT_CONFIG"
    fi
  fi
  if [[ -s "$OUT_CONFIG" ]] && [[ -n "$(tail -c 1 "$OUT_CONFIG" || true)" ]]; then
    printf '\n' >>"$OUT_CONFIG"
  fi
}

# Collapse accidental …/cc//sh_001 style paths (keep scheme ://).
normalize_url() {
  local u="$1"
  # shellcheck disable=SC2001
  echo "$u" | sed -E 's|([^:])/{2,}|\1/|g'
}

refresh_existing_indexes() {
  : >"$EXISTING_IDS_FILE"
  : >"$EXISTING_URLS_FILE"
  [[ -f "$OUT_CONFIG" ]] || return 0
  # Only real YAML keys: optional indent + channel: / url:
  awk '
    /^[[:space:]]*channel:[[:space:]]*/ {
      line=$0
      sub(/^[[:space:]]*channel:[[:space:]]*/, "", line)
      gsub(/^["'\'']|["'\'']$/, "", line)
      if (line != "") print line
    }
  ' "$OUT_CONFIG" >"$EXISTING_IDS_FILE" || true
  awk '
    /^[[:space:]]*-?[[:space:]]*url:[[:space:]]*/ {
      line=$0
      sub(/^[[:space:]]*-?[[:space:]]*url:[[:space:]]*/, "", line)
      gsub(/^["'\'']|["'\'']$/, "", line)
      if (line != "") print line
    }
  ' "$OUT_CONFIG" | while IFS= read -r u; do
    echo "$u"
    normalize_url "$u"
  done | sort -u >"$EXISTING_URLS_FILE" || true
}

url_already_present() {
  local u
  u="$(normalize_url "$1")"
  grep -qxF "$u" "$EXISTING_URLS_FILE" 2>/dev/null \
    || grep -qxF "$1" "$EXISTING_URLS_FILE" 2>/dev/null
}

is_safe_channel() {
  local id="$1"
  [[ -n "$id" && ${#id} -le 128 && "$id" =~ ^[A-Za-z0-9._-]+$ ]]
}

channel_id_taken() {
  local id="$1"
  grep -qxF "$id" "$EXISTING_IDS_FILE" 2>/dev/null
}

# Append one channel immediately (>> only).
append_one_channel() {
  local url="$1"
  local channel_id="$2"
  local source_key="$3"

  cat >>"$OUT_CONFIG" <<EOF
  # source ${source_key} is origin path only; playback name = channel → /live/${channel_id}/
  - url: "${url}"
    channel: "${channel_id}"
    enable: true
EOF
  echo "$channel_id" >>"$EXISTING_IDS_FILE"
  echo "$url" >>"$EXISTING_URLS_FILE"
  printf '%s\t%s\t%s\n' "$url" "$channel_id" "$source_key" >>"$SELECTIONS_FILE"
}

kill_vlc() {
  pkill -f "[V]LC.*${BASE_URL}/" 2>/dev/null || true
  if [[ "$(uname -s)" == "Darwin" ]]; then
    osascript -e 'tell application "VLC" to if it is running then quit' >/dev/null 2>&1 || true
  fi
  sleep 0.3
}

open_vlc() {
  local url="$1"
  kill_vlc
  "$VLC_BIN" --quiet "$url" >/dev/null 2>&1 &
  echo "  VLC opened (pid $!)"
}

ask_yn() {
  local prompt="$1"
  local ans
  while true; do
    printf "%s" "$prompt"
    if ! read -r ans; then
      echo >&2
      echo "  (EOF — stop review)" >&2
      return 2
    fi
    case "${ans}" in
      y|Y|yes|YES) return 0 ;;
      n|N|no|NO) return 1 ;;
      q|Q) return 2 ;;
      *) echo "  please enter y / n / q" ;;
    esac
  done
}

ask_channel_id() {
  local id
  while true; do
    printf "  channel id: " >&2
    if ! read -r id; then
      echo >&2
      return 1
    fi
    id="$(echo "$id" | tr -d '[:space:]')"
    if ! is_safe_channel "$id"; then
      echo "  invalid id (use A-Za-z0-9._- , max 128)" >&2
      continue
    fi
    if channel_id_taken "$id"; then
      echo "  channel id '$id' already in ${OUT_CONFIG}; pick another" >&2
      continue
    fi
    echo "$id"
    return 0
  done
}

# --- prepare output file (create once, then only append) ---
create_config_base_if_missing
normalize_pull_header
refresh_existing_indexes

existing_count="$(wc -l <"$EXISTING_IDS_FILE" | tr -d ' ')"
echo "========================================"
echo " hls2dash channel review (APPEND immediately)"
echo " VLC:     $VLC_BIN"
echo " Base:    $BASE_URL"
echo " Range:   sh_$(printf '%03d' "$START") .. sh_$(printf '%03d' "$END")  (${#STREAMS[@]} streams)"
echo " Output:  $OUT_CONFIG"
echo " Existing pull channels: ${existing_count}"
echo "========================================"
echo
echo "For each stream: watch in VLC, then answer:"
echo "  y  keep → enter channel id → written to config immediately"
echo "  n  skip"
echo "  q  quit (already-kept channels remain in config)"
echo

kept=0
skipped=0
dup_url=0
idx=0
total=${#STREAMS[@]}

for key in "${STREAMS[@]}"; do
  idx=$((idx + 1))
  url="${BASE_URL}/${key}/index.m3u8"
  echo
  echo "[${idx}/${total}] ${key}"
  echo "  url: ${url}"

  if url_already_present "$url"; then
    echo "  already in ${OUT_CONFIG} — skip (APPEND will not duplicate URL)"
    skipped=$((skipped + 1))
    dup_url=$((dup_url + 1))
    continue
  fi

  open_vlc "$url"

  yn=0
  if ask_yn "  keep this stream? [y/n/q]: "; then
    yn=0
  else
    yn=$?
  fi
  if [[ "$yn" -eq 2 ]]; then
    echo "  quit requested"
    break
  fi
  if [[ "$yn" -ne 0 ]]; then
    echo "  skipped"
    skipped=$((skipped + 1))
    continue
  fi

  if ! channel_id="$(ask_channel_id)"; then
    echo "  no channel id — quit"
    break
  fi

  append_one_channel "$url" "$channel_id" "$key"
  kept=$((kept + 1))
  echo "  kept → channel=${channel_id}  (appended to ${OUT_CONFIG})"
done

kill_vlc

echo
echo "Done."
echo "  kept this session:   ${kept}"
echo "  skipped:             ${skipped}  (already-present URLs: ${dup_url})"
echo "  config (appended):   ${OUT_CONFIG}"
echo "  session log:         ${SELECTIONS_FILE}"
echo
echo "When ready to run:"
echo "  cp ${OUT_CONFIG} ${ROOT}/config.yaml"
echo "  CONFIG=${ROOT}/config.yaml ./script/start.sh"
