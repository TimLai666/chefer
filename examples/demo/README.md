# Chefer Demo：應用 + 資料庫

一個最小但完整的範例：Python HTTP 服務（app）+ redis（db）兩個服務。
**db 直接由 chefer 從 registry 拉取**（`image: redis:7.2-alpine`，免 `docker save`）。

重點：

- ✅ **registry 拉取**：`chefer build` 直接拉官方 `redis:7.2-alpine` 打進單檔，不需先 `docker save`。
- ✅ **應用 + 資料庫**：兩個服務一起在單檔內啟動。
- ✅ **內部網路**：app 以 `127.0.0.1:6379` 連 db（同一 app 的服務共享 netns），造訪計數每次 +1。
- ✅ **資料持久化**：redis 以 AOF 持久化到 `/data`（`persist_path`，綁回 host）；關閉再執行，計數延續。
- ✅ **db 真正不對外（預設 `bridge`）**：本 app 有專屬 network namespace，db 沒有 `ports:`
  → 不會被橋接出去，從 host **連不到**（含 Windows：wslrelay 只鏡射有 relay 的宣告埠）。
  〔已在原生 Linux + WSL2 驗證；舊版「服務共享 netns、6379 仍可從 host 連到」的缺口已由 bridge 預設關閉。〕

> ⚠️ **平台注意**：官方 redis 的 entrypoint 會 `chown`/`gosu` 到 redis uid 999。
> 在 **WSL2 / macOS VM / 原生 Linux 以 root 執行** 時可行；**原生 Linux rootless**（以非 root 使用者跑單檔）
> 下單一 uid 映射會讓 chown 失敗（見專案 Roadmap 的 newuidmap 委派）。需要在 rootless 也能跑的，
> 可改回自建免-chown 的 redis（`source: tar`）。

## 一、產生 app image tar（需要 Docker）

```bash
# Linux / macOS
bash examples/demo/scripts/build-images.sh
# Windows (PowerShell)
examples\demo\scripts\build-images.ps1
```

只產生 `examples/demo/images/app.tar`（自建）；**db 不必先建**，`chefer build` 會自動從 registry 拉 redis。
打包 arm64 目標時設 `CHEFER_DEMO_PLATFORM=linux/arm64`（chefer 會拉對應架構的 redis）。

## 二、打包成單檔

```bash
cargo run -p chefer-cli -- build examples/demo/appcipe.yml --out dist
# 產物：dist/CheferDemo/CheferDemo_<target>[.exe]
```

## 三、執行與驗證

執行單檔後：

```bash
curl http://127.0.0.1:18080/        # 每次造訪計數 +1（存於 redis）
curl http://127.0.0.1:18080/        # 計數遞增
# db 不對外：以下應「連不到」（連線被拒/逾時）
curl http://127.0.0.1:6379/         # 失敗 = 預期行為（db 未對外暴露）
```

關閉後再次執行，計數會從上次的值繼續 → 證明資料持久化。
持久化資料位置：`{data_dir 或系統預設}/CheferDemo/data/db/`。

> `images/` 內的 tar 不納入版控（見 `.gitignore`）；它們由上面的 build 腳本就地產生。
