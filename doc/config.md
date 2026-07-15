# 設定說明

```bash
cp config.example.yaml config.yaml   # config.yaml 本機專用，禁止進版控
./target/release/hls2dash --config /path/to/config.yaml
CONFIG=/path/to/config.yaml ./script/start.sh
```

公開倉庫只放 [`config.example.yaml`](../config.example.yaml)；真實來源只寫本機 `config.yaml` 或環境變數。見 [layout.md 硬性規則](./layout.md)。

## 完整範例

```yaml
dash:
  listen: "0.0.0.0"
  port: 8080

cache:
  dir: "./cache"
  segment_duration_secs: 2
  window_segments: 90
  cleanup_interval_secs: 180

# 全域：所有 channel 拉流失敗後的重試間隔（秒）
reconnect_secs: 3

# 範本僅用 placeholder；真實 URL 只寫本機 config.yaml
pull:
  - url: "http://origin.example.com/live/stream/index.m3u8"
    channel: "demo"
    enable: true
```

## 欄位

| 欄位 | 必填 | 預設 | 說明 |
|------|------|------|------|
| `dash.listen` | 是 | — | HTTP bind |
| `dash.port` | 是 | — | DASH HTTP 埠 |
| `cache.dir` | 是 | — | 輸出根目錄 |
| `cache.segment_duration_secs` | 否 | `2` | DASH 切段目標秒數 |
| `cache.window_segments` | 否 | `90` | live 視窗 segment 數 |
| `cache.ttl_secs` | 否 | 自動 | janitor TTL |
| `cache.cleanup_interval_secs` | 否 | `10` | janitor 間隔 |
| `reconnect_secs` | 否 | `3` | **全域**失敗重試間隔（適用所有 channel） |
| `pull[].url` | 是* | — | HLS playlist URL |
| `pull[].channel` | 是* | — | 輸出 channel id |
| `pull[].enable` | 否 | `true` | 是否立刻拉流 |

播放：`http://<host>:8080/live/<channel>/index.mpd`
