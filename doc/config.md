# 設定說明

```bash
./target/release/hls2dash --config /path/to/config.yaml
CONFIG=/path/to/config.yaml ./script/start.sh
```

## 完整範例

```yaml
# 二選一：dash | mpegts
output_mode: mpegts

dash:
  listen: "0.0.0.0"
  port: 8080

cache:
  dir: "./cache"
  segment_duration_secs: 2
  window_segments: 90
  cleanup_interval_secs: 180

# output_mode: mpegts 時生效（對齊 trans_server + 緩衝節奏）
mpegts:
  live_holdback_segments: 6
  min_buffer_segments: 3
  max_segment_lag: 10
  send_queue: 32
  poll_interval_secs: 2
  pace_egress: true
  egress_chunk_ms: 250
  ingest_poll_factor: 0.25

reconnect_secs: 3

pull:
  - url: "http://origin.example.com/cc/sh_012/index.m3u8"
    channel: "demo"
    enable: true
```

## 欄位

| 欄位 | 必填 | 預設 | 說明 |
|------|------|------|------|
| `output_mode` | 否 | `dash` | **二選一**：`dash`（經 CMAF）或 `mpegts`（**直接** HLS→continuous TS，不經 CMAF） |
| `dash.listen` | 是 | — | HTTP bind |
| `dash.port` | 是 | — | HTTP 埠 |
| `cache.*` | — | — | CMAF 快取（兩種模式都需要內部 remux） |
| `mpegts.live_holdback_segments` | 否 | `6` | 距 live edge 的 holdback（主要 jitter buffer） |
| `mpegts.min_buffer_segments` | 否 | `3` | 至少累積幾片才開 `/mpegts` |
| `mpegts.max_segment_lag` | 否 | `10` | 落後過遠時跳到 safe edge |
| `mpegts.send_queue` | 否 | `32` | 每連線送出佇列深度 |
| `mpegts.poll_interval_secs` | 否 | `2` | 等待下一片時的 poll 間隔 |
| `mpegts.pace_egress` | 否 | `true` | 依 EXTINF 以 ~1× 媒體時間送出；`true` 時啟用分段平滑（見 [mpegts-smooth-egress.md](./mpegts-smooth-egress.md)） |
| `mpegts.egress_chunk_ms` | 否 | `250` | 平滑送出子塊目標間隔（ms，50–2000）；僅 `pace_egress: true` 時生效 |
| `mpegts.ingest_poll_factor` | 否 | `0.25` | playlist poll = TARGETDURATION × factor（越小越積極） |
| `reconnect_secs` | 否 | `3` | 全域拉流失敗重試間隔 |
| `pull[].url` | 是 | — | 來源 HLS（路徑上的 `sh_012` 等**不是**播放名） |
| `pull[].channel` | 是 | — | **播放 channel 名** → `/live/<channel>/…`（與 URL 路徑無關） |
| `pull[].enable` | 否 | `true` | 是否拉流 |

## 播放 URL

| `output_mode` | URL |
|---------------|-----|
| `dash` | `http://<host>:<port>/live/<channel>/index.mpd` |
| `mpegts` | `http://<host>:<port>/live/<channel>/mpegts` |

例：`channel: "demo"` + 來源 `…/sh_012/index.m3u8` → 播放 `…/live/demo/index.mpd`。
