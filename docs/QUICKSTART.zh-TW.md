# Chefer 快速上手（繁體中文）

本文帶你從一個現成的 Docker image，一路做到「使用者雙擊就能跑」的單一執行檔。

## 0. 安裝 Chefer

到 [GitHub Releases](https://github.com/TimLai666/chefer/releases/latest) 下載對應你作業系統的最新版壓縮包，解壓後得到：

```
chefer(.exe)        # CLI 本體
kit/                # 完整 runtime kit：
                    #   chefer-runtime-<target>  × 6 個目標平台
                    #   guest-agent-<arch>       × x86_64 / aarch64（musl 靜態）
```

把整個資料夾放在任何位置即可（CLI 會自動在自己旁邊的 `kit/` 找到所需檔案）。下載**任一**平台的包，就能打包出**所有**平台的單檔。

> 進階：kit 搜尋順序為 `--kit-dir` 參數 > 環境變數 `CHEFER_KIT_DIR` > `<chefer 所在目錄>/kit/` > `<chefer 所在目錄>/`。

## 1. 匯出 image tar

打包的素材是 Docker/OCI image tar（目前唯一支援的 image 來源）：

```bash
docker save -o images/app.tar myimage:latest
```

`docker save` 的傳統格式與 OCI archive 都可以，Chefer 依內容自動偵測；multi-arch tar 會依 `platform` 欄位自動挑選（預設 `linux/amd64`，另支援 `linux/arm64`）。

## 2. 產生並編輯 appcipe.yml

```bash
chefer init
```

會在目前目錄產生帶註解的 `appcipe.yml` 範本（不會覆蓋既有檔案）。至少要改兩個地方：

- `name:` — 應用名稱（也是輸出檔名與資料目錄名）
- `services.<服務名>.image:` — 指向步驟 1 匯出的 tar（路徑可相對於 appcipe.yml）

常用選填欄位：`ports`（埠映射 `"host:guest[/proto]"`）、`env`、`persist_path`（容器內要持久化的路徑）、`mounts`、`depends_on`、`healthcheck`、`interface_mode`。完整範例見 [examples/appcipe.yml](../examples/appcipe.yml)。

改完先驗證：

```bash
chefer check
```

## 3. 打包成單一執行檔

```bash
chefer build                                       # 打包本機平台
chefer build --target x86_64-pc-windows-msvc       # 跨平台打包（可重複 --target）
chefer build --target x86_64-unknown-linux-musl --target aarch64-apple-darwin
```

產物在 `dist/<Name>/<Name>_<target>[.exe]`。可用 `chefer inspect <檔案>` 檢視單檔內容摘要，或 `chefer run` 直接建置並執行（本機平台）。

## 4. 分發與執行

把單檔交給使用者即可——不需要 Docker、不需要安裝任何東西。雙擊（或在終端執行）就會：

1. 驗證並解壓內嵌的 bundle 到暫存目錄
2. 建立資料目錄、啟動埠映射
3. 在容器隔離環境內依 `depends_on` 順序啟動所有服務；若服務有 `healthcheck`，會等到 healthy 才繼續啟動後續服務
4. 任一服務以非 0 退出 → 整體退出並透傳 exit code（fail_fast）

### Windows 使用者注意事項

- **需要 WSL2**。沒裝過的話以系統管理員執行一次 `wsl --install` 後重開機即可；可用 `wsl --status` 確認。
- 執行時會自動建立 Chefer 專用的最小 WSL distro，app 結束後會盡力自動移除；若同一個 distro 還有另一個 Chefer app 正在使用，會保守跳過清理。不會動到你既有的 distro。
- 若要本機除錯或反覆量測啟動時間，可暫時設定 `CHEFER_KEEP_WSL_DISTRO=1` 保留 Chefer runtime distro。
- 持久化資料寫在 `%LOCALAPPDATA%\<name>\data\<service>\`（除非 appcipe 有設 `data_dir`）。
- **UDP 埠映射可用**：WSL2 自帶的 localhost 轉送只支援 TCP，故 Chefer 改由 host 端 relay 到 VM IP，再於 VM 內補 `eth0→loopback` 橋接（連只綁 loopback 的 UDP 服務也能連入）。TCP 埠（含 host≠guest 的代理）一樣正常。
- GUI 服務靠 WSLg 顯示（best-effort）。

### Linux 使用者注意事項

- 免任何依賴：使用 rootless namespaces（user/mount/pid），不需要 root、不需要安裝任何套件。
- 前提是核心允許 unprivileged user namespaces（主流發行版預設開啟）。
- 持久化資料寫在 `$XDG_DATA_HOME/<name>/` 或 `~/.local/share/<name>/`。
- GUI 服務直通 X11/Wayland socket（存在才掛載）。
- 從檔案總管下載的檔案記得 `chmod +x`（終端下載通常已有執行權限）。

### macOS

目前版本可以**在 macOS 上打包**任何平台的單檔，但 macOS 上的**執行**尚未支援（Virtualization.framework 後端規劃中），執行時會回報明確的未支援訊息。

## 常見問題

- **build 報找不到 runtime / guest-agent？** 確認 `kit/` 與 chefer 在同一目錄，或用 `--kit-dir` / `CHEFER_KIT_DIR` 指定；也可從 GitHub Releases 重新下載完整包。
- **打包 Windows / macOS 目標時要求 guest-agent？** 這兩種目標必須內嵌 musl guest-agent（Releases 的 kit 已含 x86_64 與 aarch64 兩種）。
- **服務啟動順序？** `depends_on` 會決定拓撲啟動順序；若被依賴服務有 `healthcheck`，Chefer 會等它 healthy 後才啟動後續服務。沒有 `healthcheck` 的服務仍是啟動後即視為 ready。
