# hls2dash — HLS → live MPEG-DASH (align rtmp2dash)

## Context

- **Repo**: [seantw0301/hls2dash](https://github.com/seantw0301/hls2dash)
- **Template**: sibling [seantw0301/rtmp2dash](https://github.com/seantw0301/rtmp2dash)
- **Sample origin (placeholder only)**: `http://origin.example.com/live/stream/index.m3u8` — 真實來源僅本機 `config.yaml` / 環境變數；見 [layout.md](./layout.md)

## Contract (source alignment)

| Surface | Contract |
|---------|----------|
| MPD | `http://<host>:<dash.port>/live/<channel>/index.mpd` |
| Media | `/live/<channel>/init.mp4`, `/live/<channel>/seg_N.m4s` |
| Discovery | `GET /channels` → `{"channels":[{"id","mpd"}]}` |
| Health | `GET /healthz` |
| Metrics | `GET /metrics` → `hls2dash_active_channels N` |

## Pull retry

- Fail → wait global `reconnect_secs` (default **3**) → retry forever while `enable == true`
- Stop only when `enable: false` (config or API)
- Enable/disable via API takes effect immediately

## Channel CRUD API

| Method | Path |
|--------|------|
| GET/POST | `/api/channels` |
| PUT/DELETE | `/api/channels/{id}` |
| POST | `/api/channels/{id}/enable` |
| POST | `/api/channels/{id}/disable` |
| POST | `/api/channels/enable-all` |
| POST | `/api/channels/disable-all` |

Body fields: `id`, `hls_url`, `enabled`. Reconnect delay is global `reconnect_secs` in config.

Runtime registry is in-memory; reboot reloads `config.yaml` only (API mutations not persisted in v1).

## Architecture

HLS pull → MPEG-TS demux (H.264+AAC) → CMAF `DashPackager` → HTTP DASH egress.
