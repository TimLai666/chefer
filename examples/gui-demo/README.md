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

## 在無 WSL 的 Windows 上跑（WHP 路徑）

同一個單檔在**沒有 WSL** 的 Windows 上一樣能顯示 GUI——chefer 的 **WHP 後端**用 Windows Hypervisor Platform 開一台 Linux micro-VM，VM 內以 bundle 內嵌的 **`cage`** Wayland compositor（+ Xwayland，讓 xeyes 這類 X11 程式也能跑）把畫面軟算繪（llvmpipe）到 virtio-gpu scanout，再由 helper 搬進一個原生 Windows 視窗；鍵鼠（virtio-input）與雙向文字剪貼簿都通。已於實機驗證。

需要：

- 用**含 GUI overlay + appliance 的 release kit** 打包（`chefer doctor` 會檢查 kit 是否有 `chefer-gui-overlay-<arch>`）；打 Windows 目標時 chefer 會把 GUI overlay 嵌進 bundle 的 `vm/`。純 server app 或 Linux 目標不揹這個體積。
- 執行機開啟硬體虛擬化 + WHP 選用功能。WSL 不在時自動走 WHP；機器同時有 WSL 又想強制走 WHP 時設 `CHEFER_BACKEND=whp`（長駐服務可另設 `CHEFER_WHP_TIMEOUT=<秒>` 自動收）。

其餘操作與下方相同：執行 → 出現以 app 名為標題的視窗顯示 xeyes → 關窗即結束。GPU 一律軟算繪（WHP 路徑無 GPU 加速）。

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
