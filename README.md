# hls2dash

[中文](./README_TW.md)

Pure-Rust **HLS → live MPEG-DASH or continuous MPEG-TS** service: pull live HLS (MPEG-TS), remux to CMAF, then egress as either DASH or continuous MPEG-TS (`output_mode` in config).

- **Codec**: H.264 + AAC only (passthrough, no re-encode)
- **Control**: live channel CRUD + enable/disable via HTTP API
- **Retry**: failed pulls retry on a global interval until `enable: false`
- **Egress (2選1)**:
  - `output_mode: dash` → demux → CMAF → `/live/<channel>/index.mpd`
  - `output_mode: mpegts` → **direct** HLS `.ts` stitch → `/live/<channel>/mpegts` (no CMAF; trans_server-aligned URL)
- **No ffmpeg at runtime**

**License: [MIT License](./LICENSE)**

## Build

Requires [Rust](https://rustup.rs/) (`rustc` / `cargo`).

```bash
# Debug build
cargo build

# Release build (recommended)
cargo build --release
```

Binary output:

| Mode | Path |
|------|------|
| debug | `target/debug/hls2dash` |
| release | `target/release/hls2dash` |

Run the built binary:

```bash
cp config.example.yaml config.yaml   # local only; never commit
# edit config.yaml with your own HLS URLs
./target/release/hls2dash --config config.yaml
```

`./script/start.sh` runs `cargo build --release` and then starts the process in the background.

## Quick start

```bash
cp config.example.yaml config.yaml
# edit pull: URLs in config.yaml (local file; gitignored)

# Background start / stop / restart
./script/start.sh
./script/stop.sh
./script/restart.sh

# Or run in the foreground
cargo run --release -- --config config.yaml
```

Pull example (edit local `config.yaml`; use your own HLS origin — never commit real URLs):

```yaml
pull:
  - url: "http://origin.example.com/live/stream/index.m3u8"
    channel: "demo"
    enable: true
```

After start, play: `http://127.0.0.1:8080/live/demo/index.mpd` (standard `.mpd` filename).

Smoke test (requires a reachable HLS URL via env; do not hard-code real origins):

```bash
SMOKE_HLS_URL="http://origin.example.com/live/stream/index.m3u8" ./script/smoke_test.sh
```

## Use cases

| Scenario | Description |
|----------|-------------|
| Live relay (pull) | Remote HLS URL in local `config.yaml`; pull and output live DASH |
| Multi-channel | `…/live/<channel_id>`; many pulls in parallel |
| Runtime control | HTTP API for add / update / enable / disable / delete |
| Edge / self-host | Lightweight single binary + YAML; writes to local cache |

## Config summary

Copy [`config.example.yaml`](./config.example.yaml) → `config.yaml` (gitignored). Key fields:

| Field | Description |
|-------|-------------|
| `dash.port` | DASH HTTP port |
| `cache.dir` | Segment and MPD output directory |
| `cache.segment_duration_secs` | Segment length in seconds (**default 2**) |
| `reconnect_secs` | Global pull retry interval (**default 3**) |
| `pull[].url` | Source HLS playlist URL |
| `pull[].channel` | Output channel id |

Full reference: [doc/config.md](./doc/config.md)

## Layout

| Path | Contents |
|------|----------|
| `src/` | Source code |
| `doc/` | **All documentation** |
| `script/` | Start / stop / test scripts |
| `LICENSE` | MIT license |

Dev conventions (including “≤ 1000 lines per file”): [doc/layout.md](./doc/layout.md)

## Docs

- [Doc index](./doc/README.md)
- [Architecture](./doc/architecture.md)
- [Usage / API](./doc/usage.md)
- [Config](./doc/config.md)
- [Plan](./doc/plan.md)

## Limitations (current version)

- H.264 + AAC only (MPEG-TS HLS)
- DASH over HTTP (HTTPS can be added later)
- Playlist filename is standard `index.mpd`
- API channel mutations are in-memory; reboot reloads `config.yaml` only
