# 專案結構與硬性規範

本文件定義 `hls2dash` 的目錄約定與開發限制。

## 目錄約定

| 路徑 | 用途 |
|------|------|
| `src/` | Rust 原始碼 |
| `doc/` | **所有文件** |
| `script/` | 啟動 / 停止 / 測試 |
| `config.example.yaml` | **公開**設定範本（僅 placeholder URL） |
| `config.yaml` | 本機執行設定（**禁止進版控**） |
| `cache/` | 執行期 DASH 輸出（勿進版控） |
| `LICENSE` | MIT |

## 硬性規則

1. **文件**：說明文件一律放 `doc/`（根目錄僅 `README.md` / `README_TW.md`）
2. **腳本**：啟動、停止、測試等腳本一律放 `script/`
3. **單檔行數上限**：每一支 `.rs` **不可超過 1000 行**
4. **授權**：MIT License
5. **`config.yaml` 禁止進版控**：必須列在 `.gitignore`；公開倉庫只允許 `config.example.yaml`。本機請 `cp config.example.yaml config.yaml` 後再改。
6. **禁止真實串流位置**：任何會進 git 的檔案（含 `config.example.yaml`、`doc/`、`script/`、`README*`、程式碼註解）**不可**寫入真實 HLS/RTMP origin IP、hostname、路徑或可還原的 production URL。僅可用明顯假資料（如 `origin.example.com`）。真實來源只能放本機 `config.yaml` 或環境變數（如 `SMOKE_HLS_URL` / `BASE_URL`）。
7. **歷史同樣適用**：若誤提交真實來源，必須改寫歷史並 force-push 清掉公開 record，不可只靠後續 commit 「蓋掉」。

## 模組

| 模組 | 職責 |
|------|------|
| `config` | 讀取 / 驗證 YAML |
| `channel` | ChannelRegistry CRUD + enable |
| `hls` | playlist poll、下載、重試迴圈 |
| `demux` | MPEG-TS → AccessUnit |
| `dash` | CMAF + live `index.mpd` |
| `http` | DASH 服務 + `/api/channels` |
| `cache` | TTL janitor |
