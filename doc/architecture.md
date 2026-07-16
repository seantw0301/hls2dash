# 架構說明

`hls2dash` 是純 Rust 的 **HLS → live egress** 服務：從 HTTP(S) HLS（MPEG-TS）拉流，再依 `output_mode` **二選一** 輸出：

- `dash` → 內部 demux → CMAF → live MPEG-DASH（`index.mpd`）
- `mpegts` → **直接** stitch HLS `.ts` 成 continuous MPEG-TS（**不經 CMAF**），路徑對齊 trans_server：`/live/<channel>/mpegts`

無 RTMP 推流埠。

## 資料流

```text
遠端 HLS (config / API)
  http(s)://…/index.m3u8 + .ts
         │
         ▼
┌─────────────────┐
│ HLS Pull Worker │  ← enable=false 即取消；失敗每 reconnect_secs 重試
└────────┬────────┘
         │
   output_mode?
    ├─ dash
    │    demux → CMAF Packager → cache/…/init.mp4 + seg_N.m4s
    │    → HTTP /live/<ch>/index.mpd
    └─ mpegts
         TsStitcher（CC rewrite + seg_N.dur）→ cache/…/seg_N.ts
         → HTTP /live/<ch>/mpegts
           (min_buffer → holdback jitter buffer → paced 1× egress)
           來源 HLS 卡頓時由磁碟 buffer 吸收，不因 ingest 延遲而 burst
```

### mpegts 緩衝與流速

1. **Ingest**：較密的 playlist poll（`ingest_poll_factor`），啟動時多抓 holdback+min_buffer 片填滿磁碟緩衝。
2. **Buffer**：`live_holdback_segments` 讓客戶端落在 live edge 後方；`min_buffer_segments` 未滿前回 503。
3. **Egress**：依 `seg_N.dur`（EXTINF）以約 1× realtime 送出；underrun 恢復後重新錨定節奏，不一次吐多片。

## 控制面

- Boot：`config.yaml` 的 `pull:` 寫入記憶體 `ChannelRegistry`
- 執行期：`/api/channels*` CRUD / enable / disable（不回寫 YAML）
- `GET /channels`：依模式回傳 `mpd` 或 `mpegts` 路徑

## Codec

僅 **H.264（AVC）+ AAC**（dash 模式 demux 時強制）。mpegts 模式原樣 stitch origin TS。

## 重試

任一拉流失敗 → 記錄錯誤 → sleep 全域 `reconnect_secs`（預設 3）→ 重試。僅 `enable=false` 結束 worker。
