# Chefer Demo：應用 + 資料庫

一個最小但完整的範例：Python HTTP 服務（app）+ redis（db）兩個服務。

實測（Windows + WSL2，2026-06）結果：

- ✅ **應用 + 資料庫**：兩個服務一起在單檔內啟動。
- ✅ **內部網路**：app 以 `127.0.0.1:6379` 連 db（同一 app 的服務共享網路 namespace），
  造訪計數每次 +1，證明 app↔db 連線正常。
- ✅ **資料持久化**：redis 以 AOF 持久化到 `/data`（`persist_path`，綁回 host）；
  關閉單檔再重新執行，計數從上次延續（實測 …→5 →重啟→ 6）。
- ⚠️ **db 不對外暴露 — v1 尚未真正達成**：chefer **不會**為沒有 `ports:` 的 db
  建立 host→guest 代理；但 chefer v1 的服務都共享同一個網路 namespace（無逐 app 網路隔離），
  且 **WSL2 的 `wslrelay` 會把 VM 內任何 loopback 監聽埠自動鏡射到 Windows localhost**，
  因此實測 db 的 6379 仍可從 Windows 連到。要真正做到「內部專用、不可從外部連入」，
  需要替每個 app 建立獨立 network namespace（只把宣告的 `ports:` 橋接出去）——這是 v1 的已知缺口。
  在原生 Linux 上 db 也僅綁 loopback（不對 LAN 暴露），但仍可從同機 host 連到；同樣需要
  netns 隔離才能完全內部化。

> 這個 demo 的價值之一就是把上述 networking 缺口實測出來：app+db+內部通訊+持久化在 Windows 上
> 確實可運作，但「完全不對外」需要 chefer 加上逐 app 網路隔離後才成立。

## 一、產生 image tar（需要 Docker）

```bash
# Linux / macOS
bash examples/demo/scripts/build-images.sh
# Windows (PowerShell)
examples\demo\scripts\build-images.ps1
```

產生 `examples/demo/images/app.tar`（自建）與 `examples/demo/images/db.tar`（redis 官方映像）。
打包 arm64 目標時設 `CHEFER_DEMO_PLATFORM=linux/arm64`。

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
