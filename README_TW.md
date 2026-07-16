# hls2dash

[English](./README.md)

純 Rust 的 **HLS → live MPEG-DASH 或連續 MPEG-TS** 服務：拉取 live HLS，內部 remux 成 CMAF，再依 `output_mode` 二選一輸出。

- **Codec**：固定 H.264 + AAC（passthrough，不重編碼）
- **控制**：HTTP API 即時 CRUD / enable / disable
- **重試**：拉流失敗依全域間隔重試，直到 `enable: false`
- **輸出（二選一）**：
  - `output_mode: dash` → demux → CMAF → `/live/<channel>/index.mpd`
  - `output_mode: mpegts` → **直接** HLS `.ts` stitch → `/live/<channel>/mpegts`（不經 CMAF）
- **執行期不依賴 ffmpeg**

**授權：[MIT License](./LICENSE)** — 本專案依 MIT 開源。

## Build

需求：已安裝 [Rust](https://rustup.rs/)（`rustc` / `cargo`）。

```bash
# 開發建置（debug）
cargo build

# 發行建置（建議）
cargo build --release
```

產出二元檔：

| 模式 | 路徑 |
|------|------|
| debug | `target/debug/hls2dash` |
| release | `target/release/hls2dash` |

直接執行建置結果：

```bash
cp config.example.yaml config.yaml   # 僅本機；禁止提交
# 編輯 config.yaml，填入你自己的 HLS URL
./target/release/hls2dash --config config.yaml
```

`./script/start.sh` 會自動執行 `cargo build --release` 後再背景啟動。

## 快速開始

```bash
cp config.example.yaml config.yaml
# 編輯 config.yaml 的 pull:（本機檔；已 gitignore）

# 背景啟動 / 停止 / 重啟
./script/start.sh
./script/stop.sh
./script/restart.sh

# 或前景執行
cargo run --release -- --config config.yaml
```

拉流範例（編輯本機 `config.yaml`；請用你自己的來源，**勿提交真實 URL**）：

```yaml
pull:
  - url: "http://origin.example.com/live/stream/index.m3u8"
    channel: "demo"
    enable: true
```

啟動後播放：`http://127.0.0.1:8080/live/demo/index.mpd`（檔名為標準 `.mpd`）。

煙霧測試（需透過環境變數提供可連線的 HLS URL；勿把真實來源寫進倉庫）：

```bash
SMOKE_HLS_URL="http://origin.example.com/live/stream/index.m3u8" ./script/smoke_test.sh
```

## 用途

| 場景 | 說明 |
|------|------|
| 直播轉發（拉流） | 由本機 `config.yaml` 指定遠端 HLS URL，主動拉取並輸出 DASH |
| 多頻道 | `…/live/<channel_id>`；多路拉流可並行 |
| 執行期控制 | HTTP API 新增 / 更新 / 啟用 / 停用 / 刪除 |
| 邊緣 / 自架 | 輕量單二元檔 + YAML，輸出寫入本機 cache |

## 設定摘要

複製 [`config.example.yaml`](./config.example.yaml) → `config.yaml`（已 gitignore）。重點欄位：

| 欄位 | 說明 |
|------|------|
| `dash.port` | DASH HTTP 埠 |
| `cache.dir` | 切片與 MPD 輸出目錄 |
| `cache.segment_duration_secs` | 切片長度（秒，**預設 2**） |
| `reconnect_secs` | 全域拉流重試間隔（**預設 3**） |
| `pull[].url` | 來源 HLS playlist URL |
| `pull[].channel` | 輸出 channel id |

完整說明：[doc/config.md](./doc/config.md)

## 目錄

| 路徑 | 內容 |
|------|------|
| `src/` | 程式原始碼 |
| `doc/` | **全部文件** |
| `script/` | 啟動 / 停止 / 測試腳本 |
| `LICENSE` | MIT 授權 |

開發規範（含「單檔 ≤ 1000 行」）：[doc/layout.md](./doc/layout.md)

## 文件

- [文件索引](./doc/README.md)
- [架構](./doc/architecture.md)
- [用法 / API](./doc/usage.md)
- [設定](./doc/config.md)
- [計畫](./doc/plan.md)

## 限制（目前版本）

- 僅 H.264 + AAC（MPEG-TS HLS）
- DASH 為 HTTP（HTTPS 後續可加）
- 播放清單檔名為標準 `index.mpd`
- API 變更僅存記憶體；重開機會從 `config.yaml` 重新載入
