# Chefer GUI Demo：把圖形程式打包成單檔

一個最小但完整的範例：把一個 X11 圖形程式（**xeyes**，一對會追游標的眼睛）打包成單一執行檔，用來驗證 chefer 的 **GUI 支援**。

xeyes 是最適合做 GUI 驗證的程式：它一開機就嘗試連 `$DISPLAY` 的 X server，**連不到就立刻以非 0 退出**。chefer 的 `crash: fail_fast` 會讓任一服務非 0 退出就整個收掉——所以「視窗出現且 app 持續運行」本身就是 GUI 鏈路通了的證明。

實測（Windows + WSL2/WSLg，2026-06）：

- ✅ **GUI 視窗顯示**：執行單檔後，桌面出現 xeyes 視窗（眼睛跟著滑鼠轉）。
- ✅ **X11 連線**：`interface_mode: gui` 讓 chefer 把 host 的 X11 socket 掛進容器
  （Windows 經 WSLg：distro 內 `/tmp/.X11-unix` → `/mnt/wslg/.X11-unix`）並設 `DISPLAY=:0`；
  xeyes 成功連上、未觸發 fail_fast。

> Linux 上同樣可用：chefer 直通 `/tmp/.X11-unix` 與 `$XDG_RUNTIME_DIR/wayland-*`，
> 故 X11 與 Wayland 程式都能顯示（best-effort，視 host 是否有 X/Wayland server）。

## 一、產生 image tar（需要 Docker）

```bash
# Linux / macOS
bash examples/gui-demo/scripts/build-images.sh
# Windows (PowerShell)
examples\gui-demo\scripts\build-images.ps1
```

產生 `examples/gui-demo/images/gui.tar`（debian-slim + x11-apps）。
打包 arm64 目標時設 `CHEFER_DEMO_PLATFORM=linux/arm64`。

## 二、打包成單檔

```bash
cargo run -p chefer-cli -- build examples/gui-demo/appcipe.yml --out dist
# 產物：dist/CheferGuiDemo/CheferGuiDemo_<target>[.exe]
```

## 三、執行與驗證

直接執行單檔：

```bash
./dist/CheferGuiDemo/CheferGuiDemo_<target>
```

- **Windows**：需要 Windows 11（內建 WSLg）。執行後桌面會跳出 xeyes 視窗。
- **Linux**：需有執行中的 X11 或 Wayland session（桌面環境）。執行後出現 xeyes 視窗。

關掉視窗（或 Ctrl-C）即結束。看到眼睛 = chefer 的 GUI 支援正常。

> `images/` 內的 tar 不納入版控（見 `.gitignore`）；由上面的 build 腳本就地產生。
