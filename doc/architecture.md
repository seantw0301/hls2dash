# 架構說明

`hls2dash` 是純 Rust 的 **HLS → live MPEG-DASH** 服務：從 HTTP(S) HLS（MPEG-TS）拉流，remux 成 live MPEG-DASH（不重編碼）。無 RTMP 推流埠。

## 資料流

```text
遠端 HLS (config / API)
  http(s)://…/index.m3u8 + .ts
         │
         ▼
┌─────────────────┐
│ HLS Pull Worker │  ← enable=false 即取消；失敗每 3s 重試
└────────┬────────┘
         ▼
┌─────────────────┐
│ MPEG-TS Demux   │  H.264 + AAC → AccessUnit
└────────┬────────┘
         ▼
┌─────────────────┐
│ CMAF Packager   │
└────────┬────────┘
         ▼
  cache/live/<channel>/
    init.mp4 / seg_N.m4s / index.mpd
         ▼
  HTTP DASH egress（與 rtmp2dash 相同路徑）
```

## 控制面

- Boot：`config.yaml` 的 `pull:` 寫入記憶體 `ChannelRegistry`
- 執行期：`/api/channels*` CRUD / enable / disable（不回寫 YAML）
- `GET /channels`：rtmp2dash 相容發現（目前 enabled 且 running 的頻道）

## Codec

僅 **H.264（AVC）+ AAC**。其他 track 略過。媒體為 passthrough remux。

## 重試

任一拉流失敗 → 記錄錯誤 → sleep 全域 `reconnect_secs`（預設 3）→ 重試。僅 `enable=false` 結束 worker。
