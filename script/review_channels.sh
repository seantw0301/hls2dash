#!/usr/bin/env bash
# Interactive VLC review for a numeric HLS range, then APPEND selections
# into an existing hls2dash config.yaml (create base file if missing).
#
# Example:
#   BASE_URL=http://origin.example.com/cc ./script/review_channels.sh 20 50
#   → play sh_020 .. sh_050
#   → kept channels are appended under pull: in config.yaml
#
# For each stream:
#   - open in VLC
#   - y → enter channel id (e.g. demo), keep this stream
#   - n → skip to next
#
# Usage:
#   BASE_URL=http://origin.example.com/cc ./script/review_channels.sh 20 50
#   BASE_URL=http://origin.example.com/cc ./script/review_channels.sh -o /path/to/config.yaml 20 50
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_CONFIG="${ROOT}/config.yaml"
SELECTIONS_FILE="${ROOT}/script/.review_selections.tsv"

usage() {
  cat <<EOF
Usage: BASE_URL=<hls-base> $(basename "$0") [-o config.yaml] START END

  Interactively play HLS streams sh_START .. sh_END in VLC.
  Kept channels are APPENDED to the existing config.yaml pull: list.

  Example:
    BASE_URL=http://origin.example.com/cc $(basename "$0") 20 50     # sh_020 .. sh_050
    BASE_URL=http://origin.example.com/cc $(basename "$0") 100 120   # sh_100 .. sh_120

  Prompt: y = keep (then enter channel id), n = skip

Options:
  -o FILE   Config path to append into (default: ${ROOT}/config.yaml)
  -h        Show help

Env:
  BASE_URL  Required. HLS base prefix.
            URL pattern: \${BASE_URL}/sh_XXX/index.m3u8
EOF
}

if [[ -z "${BASE_URL:-}" ]]; then
  echo "error: BASE_URL is required (do not hard-code real origins in this script)" >&2
  usage
  exit 1
fi

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

mkdir -p "$(dirname "$SELECTIONS_FILE")"
: >"$SELECTIONS_FILE"

EXISTING_IDS_FILE="$(mktemp)"
trap 'rm -f "$EXISTING_IDS_FILE"' EXIT

# Collect channel ids already present in target config (for duplicate checks).
if [[ -f "$OUT_CONFIG" ]]; then
  awk '
    /^[[:space:]]*channel:[[:space:]]*/ {
      line=$0
      sub(/^[[:space:]]*channel:[[:space:]]*/, "", line)
      gsub(/^["'\'']|["'\'']$/, "", line)
      if (line != "") print line
    }
  ' "$OUT_CONFIG" >"$EXISTING_IDS_FILE" || true
fi

echo "========================================"
echo " hls2dash channel review"
echo " VLC:     $VLC_BIN"
echo " Base:    $BASE_URL"
echo " Range:   sh_$(printf '%03d' "$START") .. sh_$(printf '%03d' "$END")  (${#STREAMS[@]} streams)"
echo " Append:  $OUT_CONFIG"
echo "========================================"
echo
echo "For each stream: watch in VLC, then answer:"
echo "  y  keep → enter channel id (e.g. demo)"
echo "  n  skip"
echo

is_safe_channel() {
  local id="$1"
  [[ -n "$id" && ${#id} -le 128 && "$id" =~ ^[A-Za-z0-9._-]+$ ]]
}

channel_id_taken() {
  local id="$1"
  if grep -qxF "$id" "$EXISTING_IDS_FILE" 2>/dev/null; then
    return 0
  fi
  if awk -F'\t' -v id="$id" '$2 == id { found=1 } END { exit !found }' "$SELECTIONS_FILE" 2>/dev/null; then
    return 0
  fi
  return 1
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
    read -r ans || exit 1
    case "${ans}" in
      y|Y|yes|YES) return 0 ;;
      n|N|no|NO) return 1 ;;
      *) echo "  please enter y or n" ;;
    esac
  done
}

ask_channel_id() {
  local id
  while true; do
    printf "  channel id: "
    read -r id || exit 1
    id="$(echo "$id" | tr -d '[:space:]')"
    if ! is_safe_channel "$id"; then
      echo "  invalid id (use A-Za-z0-9._- , max 128)"
      continue
    fi
    if channel_id_taken "$id"; then
      echo "  channel id '$id' already used; pick another"
      continue
    fi
    echo "$id"
    return 0
  done
}

# Ensure config exists with a pull: section so we can append.
ensure_config_base() {
  if [[ -f "$OUT_CONFIG" ]]; then
    if grep -qE '^[[:space:]]*pull:[[:space:]]*(\[\][[:space:]]*)?$' "$OUT_CONFIG" \
      || grep -qE '^pull:' "$OUT_CONFIG"; then
      return 0
    fi
    echo "error: $OUT_CONFIG has no top-level pull: section to append into" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$OUT_CONFIG")"
  cat >"$OUT_CONFIG" <<'HDR'
dash:
  listen: "0.0.0.0"
  port: 8080

cache:
  dir: "./cache"
  segment_duration_secs: 2
  window_segments: 90
  cleanup_interval_secs: 180

reconnect_secs: 3

pull:
HDR
  echo "Created new config: $OUT_CONFIG"
}

# Replace `pull: []` with `pull:` so list items can be appended.
normalize_pull_header() {
  if grep -qE '^pull:[[:space:]]*\[\][[:space:]]*$' "$OUT_CONFIG"; then
    # macOS/BSD sed needs backup suffix; Linux accepts empty.
    if sed --version >/dev/null 2>&1; then
      sed -i 's/^pull:[[:space:]]*\[\][[:space:]]*$/pull:/' "$OUT_CONFIG"
    else
      sed -i '' 's/^pull:[[:space:]]*\[\][[:space:]]*$/pull:/' "$OUT_CONFIG"
    fi
  fi
  # Ensure file ends with a newline before appending.
  if [[ -s "$OUT_CONFIG" ]] && [[ -n "$(tail -c 1 "$OUT_CONFIG" || true)" ]]; then
    printf '\n' >>"$OUT_CONFIG"
  fi
}

idx=0
total=${#STREAMS[@]}
for key in "${STREAMS[@]}"; do
  idx=$((idx + 1))
  url="${BASE_URL}/${key}/index.m3u8"
  echo
  echo "[${idx}/${total}] ${key}"
  echo "  url: ${url}"

  open_vlc "$url"

  if ask_yn "  keep this stream? [y/n]: "; then
    channel_id="$(ask_channel_id)"
    # TSV: url<TAB>channel_id<TAB>source_key
    printf '%s\t%s\t%s\n' "$url" "$channel_id" "$key" >>"$SELECTIONS_FILE"
    echo "  kept → channel=${channel_id}"
  else
    echo "  skipped"
  fi
done

kill_vlc

count="$(wc -l <"$SELECTIONS_FILE" | tr -d ' ')"
echo
if [[ "$count" -eq 0 ]]; then
  echo "No channels selected; config unchanged: $OUT_CONFIG"
  exit 0
fi

echo "Selected ${count} channel(s). Appending to ${OUT_CONFIG} ..."
ensure_config_base
normalize_pull_header

while IFS=$'\t' read -r url channel_id source_key; do
  [[ -z "$url" ]] && continue
  cat >>"$OUT_CONFIG" <<EOF
  - url: "${url}"
    channel: "${channel_id}"
    enable: true
EOF
done <"$SELECTIONS_FILE"

echo
echo "Done."
echo "  session selections: ${SELECTIONS_FILE}"
echo "  config (appended):  ${OUT_CONFIG}"
echo
echo "Start hls2dash with:"
echo "  CONFIG=${OUT_CONFIG} ./script/start.sh"
