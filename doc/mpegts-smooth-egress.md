# MPEG-TS 分段平滑送出

`output_mode: mpegts` 且 `pace_egress: true` 時，streamer 會把每一片 HLS segment **均勻切成多個子塊** 再送出，而不是整片 burst 後長睡。目標是讓 `/live/<channel>/mpegts` 的位元率接近恆定，避免播放器週期性卡頓。

實作位於 `src/mpegts/streamer.rs`（`SmoothSegment`、`plan_smooth_slices`）。

## 問題背景

典型 CCTV HLS 來源：

- `EXTINF` 約 **11s**（非固定 2s）
- 單片 `.ts` 大小可變（約 0.8–2.6 MB）

舊版 `pace_egress` 行為：

1. 讀取整個 `seg_N.ts`
2. **一次** 寫入 HTTP 連線（burst）
3. `sleep(seg_N.dur)` 等待下一媒體時間

客戶端觀感：每 ~11s 收到一大包資料，其餘時間幾乎無流量 → 監控上出現 **~10s 的 stall**，播放器易卡頓。

## 解法：分段平滑送出

啟用 `pace_egress` 後，每片 segment 進入 `SmoothSegment` 狀態機：

```text
seg_N.ts（磁碟）
    │
    ▼
plan_smooth_slices(len, EXTINF, egress_chunk_ms)
    │  → slice_bytes（188-byte 對齊）
    │  → slice_interval = EXTINF / slice_count
    ▼
迴圈：wait(slice_interval) → take_slice() → tx.send(chunk)
    │
    ▼
HTTP /mpegts（近似恆定位元率）
```

### 切片演算法（`plan_smooth_slices`）

| 步驟 | 說明 |
|------|------|
| 時長 | `dur_secs` 限制在 0.1–60s |
| 目標間隔 | `chunk_ms` → 秒（演算法內再限制 50ms–2s） |
| 片數 | `ceil(dur / chunk_secs)`，至少 1，至多 `data_len / 188` |
| 每片位元組 | `data_len / slices`，向下對齊 **188-byte TS packet** |
| 間隔 | `EXTINF / slices` |

子塊邊界永遠落在 TS sync byte 對齊位置，避免切斷封包。

### 與緩衝的關係

分段平滑只影響 **egress 節奏**，不改變 ingest / holdback 語意：

| 階段 | 行為 |
|------|------|
| Ingest | 較密 playlist poll，先填滿 `min_buffer` + holdback |
| Buffer | 客戶端落在 live edge 後方 `live_holdback_segments` 片 |
| Egress | 每片 segment 在 `EXTINF` 內均勻送出；underrun 恢復後重新錨定，不追趕 burst |

空 segment（僅 `.dur`、無 `.ts`，例如 skip 後的間隙）仍只 sleep，不送資料。

## 設定

```yaml
mpegts:
  pace_egress: true      # 必須為 true 才啟用平滑送出
  egress_chunk_ms: 250   # 目標子塊間隔（ms），有效範圍 50–2000
```

| 欄位 | 預設 | 說明 |
|------|------|------|
| `pace_egress` | `true` | `false` 時整片一次送出（舊 burst 行為） |
| `egress_chunk_ms` | `250` | 越小 → 子塊越多、流量越平滑；過小會增加 wake 次數 |

調校建議：

- **一般直播**：`250`（預設）即可
- **EXTINF 很長（>10s）且仍見微頓**：可試 `150–200`
- **CPU / 連線數很多**：可試 `400–500`，略增 burst 但降低 timer 頻率

## 驗證方式

對 `/mpegts` 端點做短時間取樣（例如 60s），觀察 chunk 間隔與 stall：

| 指標 | 舊行為（整片 burst） | 平滑送出後 |
|------|----------------------|------------|
| ≥2s stall | 週期性 ~10s | **0** |
| 平均 bitrate | 尖峰 + 長空檔 | 接近恆定 |
| chunk 間隔 | ~segment 長度 | ~`egress_chunk_ms` 量級（0.5–1s） |

專案內有 `plan_smooth_slices` 單元測試；部署後可用本機腳本或 `curl` 連續讀取並記錄時間戳驗證。

## 相關文件

- [architecture.md](./architecture.md) — 整體 mpegts 資料流
- [config.md](./config.md) — 完整設定欄位
