# 用法

## 建置 / 啟動

```bash
cp config.example.yaml config.yaml   # 本機專用，禁止提交
# 編輯 config.yaml 填入你的 HLS URL
cargo build --release
./script/start.sh
./script/stop.sh
./script/restart.sh
```

健康檢查：

```bash
curl -s http://127.0.0.1:8080/healthz
curl -s http://127.0.0.1:8080/channels
```

播放：

- DASH（`output_mode: dash`）：`http://127.0.0.1:8080/live/demo/index.mpd`
- MPEG-TS（`output_mode: mpegts`）：`http://127.0.0.1:8080/live/demo/mpegts`


## Channel API

```bash
# 列表
curl -s http://127.0.0.1:8080/api/channels

# 新增
curl -s -X POST http://127.0.0.1:8080/api/channels \
  -H 'content-type: application/json' \
  -d '{"id":"demo","hls_url":"http://origin.example.com/live/stream/index.m3u8","enabled":true}'

# 停用 / 啟用（即時）
curl -s -X POST http://127.0.0.1:8080/api/channels/demo/disable
curl -s -X POST http://127.0.0.1:8080/api/channels/demo/enable

# 更新 URL / enable
curl -s -X PUT http://127.0.0.1:8080/api/channels/demo \
  -H 'content-type: application/json' \
  -d '{"hls_url":"http://origin.example.com/live/stream/index.m3u8","enabled":true}'

# 刪除
curl -s -X DELETE http://127.0.0.1:8080/api/channels/demo
```

失敗時 worker 依全域 `reconnect_secs`（預設 3 秒）重試；僅 `disable` / `enable:false` 會停止。

## 煙霧測試

需自行提供可連線的 HLS URL（勿把真實來源寫進倉庫）：

```bash
SMOKE_HLS_URL="http://origin.example.com/live/stream/index.m3u8" ./script/smoke_test.sh
```

## 解碼層監控（MPEG-TS）

錄製 N 秒 `/mpegts`，用 ffprobe 檢查幀 PTS gap，再用 ffmpeg 全解碼抓 SPS/corrupt 等 warning。需本機有 `ffmpeg` / `ffprobe`：

```bash
./script/monitor_decode_ffprobe.py http://127.0.0.1:8080/live/demo/mpegts 180
MPEGTS_URL="http://127.0.0.1:8080/live/demo/mpegts" ./script/monitor_decode_ffprobe.py
```

exit 0 = 大致正常；exit 2 = 解碼層有明顯幀 gap 或大量 warning。

## 互動選台（VLC）

指定範圍（例如 `20 50` → `sh_020`–`sh_050`），逐一用 VLC 播放；選 `y` 並輸入 channel id 後，**append** 到 `config_pre.yaml`（既有 `pull:` **不會被覆寫**；僅第一次不存在時建立骨架）。

必須設定 `BASE_URL`（你的 HLS 來源前綴）：

```bash
BASE_URL="http://origin.example.com/cc" ./script/review_channels.sh 20 50
BASE_URL="http://origin.example.com/cc" ./script/review_channels.sh 100 120
# 自訂輸出路徑（預設已是 config_pre.yaml）
BASE_URL="http://origin.example.com/cc" ./script/review_channels.sh -o ./config_pre.yaml 1 20
```

確認後再套用到執行設定：

```bash
cp config_pre.yaml config.yaml
./script/start.sh
```
