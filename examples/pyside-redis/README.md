# Chefer Demo：PySide6 GUI + Redis

一個真實一點的 GUI 範例：**PySide6（Qt6）視窗 + redis**，兩個容器打成一個單檔。
視窗顯示存在 redis 的點擊計數，按鈕 INCR、每秒自動刷新。

- 視窗出現 = GUI 鏈路通（容器內 Qt/xcb 連到 host 的 X server；Windows 經 WSLg）。
- 計數能讀寫 = 內部網路連到 db（redis）正常（app 以 `127.0.0.1:6379` 連 db）。
- `crash: fail_fast`：任一服務非零結束 → 整個 app 收掉，不會卡著半死。

> GUI 映像含整套 Qt，**較大**（打包出的單檔約數百 MB），屬正常。

> ✅ **db 真正不對外（預設 `bridge`）**：`db` 沒有 `ports:`，本 app 有專屬 network namespace，
> 故 redis 從 host **連不到**（含 Windows：wslrelay 只鏡射有 relay 的宣告埠）。
> 〔舊版「服務共用 netns、6379 仍可從 host 連到」的缺口已由 bridge 預設關閉。〕
> db 直接由 chefer 從 registry 拉官方 `redis:7.2-alpine`（免 docker save）。

## 一、產生 GUI image tar（需要 Docker）

```bash
# Linux / macOS
bash examples/pyside-redis/scripts/build-images.sh
# Windows (PowerShell)
examples\pyside-redis\scripts\build-images.ps1
```

只產生 `images/gui.tar`（python-slim + PySide6）；**db 不必先建**，`chefer build` 會自動從 registry 拉 redis。

## 二、打包成單檔

```bash
cargo run -p chefer-cli -- build examples/pyside-redis/appcipe.yml --out dist
# 產物：dist/CheferPysideRedis/CheferPysideRedis_<target>[.exe]
```

## 三、執行

```bash
./dist/CheferPysideRedis/CheferPysideRedis_<target>
```

- **Windows**：需 Windows 11（內建 WSLg）。執行後桌面跳出 Qt 視窗；按按鈕計數 +1。
- **Linux**：需有執行中的 X11/Wayland session。

關掉視窗或 Ctrl-C 即結束。資料（計數）持久化在 `{data_dir 或系統預設}/CheferPysideRedis/data/db/`，
重開後計數延續。

> `images/` 內的 tar 不納入版控（見 `.gitignore`）；由上面的 build 腳本就地產生。
