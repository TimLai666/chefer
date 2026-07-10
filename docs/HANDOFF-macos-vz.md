# Handoff：在 Mac 上驗證 macOS vz 後端

這份是給「拿到一台實體 Mac、要把 chefer 的 macOS（Virtualization.framework / vz）路徑收尾」的接手文件。程式碼側都寫完了，剩下的**只差在真 Mac 上跑一次驗證**——GitHub 的 macOS runner 是巢狀虛擬化、開不了 VZ guest，所以 CI 只能編譯+簽章，端到端一定要實機。

## TL;DR

```bash
# 1. 前置：Xcode CLT、Rust、（建 guest-agent 用）cross + Docker；備一份含 appliance/overlay 的 kit
# 2. 一鍵驗證（非 GUI 的核心路徑：開機 / exit code 傳播 / 埠轉發）
bash scripts/vz-smoke.sh --kit-dir <放 appliance 的資料夾>
# 3. 加 --gui 再驗顯示 / HID / 剪貼簿 / 動態解析度 / 關窗語意（會列手動檢核清單）
bash scripts/vz-smoke.sh --kit-dir <...> --gui
```

腳本自己會**從當前原始碼**建 `chefer-vz-helper`（swiftc + virtualization entitlement 簽章）、`chefer-cli`、`chefer-runtime`、以及 `guest-agent`（cross，含本輪的 GUI 修復），只從 `--kit-dir` 拿兩個「需要 Docker 才建得出、且本輪沒改」的 Linux 產物：**appliance kernel/initramfs** 與 **GUI overlay squashfs**。

---

## 現在程式碼在什麼狀態

已實作、CI（swiftc 編譯+entitlement 檢查）把關、**但端到端待實機**：

- **vz 開機路徑**（`crates/vmm-backend/src/vz.rs` + `vz-helper/main.swift`）：以 bundle 內嵌 Linux appliance 開一台 micro-VM，virtiofs 共享 bundle/data，guest-agent 在裡面跑；解析 console 的 `CHEFER_GUEST_IP` / `CHEFER_GUEST_EXIT` 標記做埠轉發與 exit code 傳播。實機驗證前 `availability()` 預設不可用，需 `CHEFER_VZ_EXPERIMENTAL=1`。
- **macOS GUI host 側**（`vz-helper --gui`）：virtio-gpu scanout（1280×800 預設，`CHEFER_VZ_GUI_SIZE=WxH` 覆寫）+ USB 鍵盤/絕對座標指標 + AppKit 視窗承載 `VZVirtualMachineView`；**關窗＝app 結束**（同 WHP 語意）。macOS 14+ 開 `.resizable` + `automaticallyReconfiguresDisplay`。
- **剪貼簿**（host↔guest，文字 + PNG）：與 WHP 同一線協定 + cmdline token；host 端經 VZ NAT 直連 guest IP:55381，NSPasteboard 讀寫 `.png`/`.tiff`。

roadmap 上其餘純硬體項（與 Mac 無關，不在本 handoff）：AMD/Intel GPU、多卡硬隔離——那些要對應的 Linux GPU 機器。

---

## 前置需求

| 需求 | 用途 | 備註 |
|---|---|---|
| macOS **13+**（GUI 動態解析度需 **14+**） | Virtualization.framework / virtiofs | Apple Silicon 或 Intel 皆可 |
| Xcode Command Line Tools | `swiftc` + `codesign`（建/簽 vz-helper） | `xcode-select --install` |
| Rust toolchain | 建 cli/runtime | https://rustup.rs |
| **cross + Docker** | 從原始碼建 `guest-agent`（含 GUI 修復） | `cargo install cross`；沒有的話腳本退回 kit 的 guest-agent 並警告 |
| 一份 **kit**（appliance + overlay） | vz-smoke 從這裡拿 vmlinuz/initramfs/overlay | 見下節 |

### 取得 kit（appliance + gui-overlay）

腳本只需要 kit 裡這幾個檔（`<arch>` = Apple Silicon 為 `aarch64`、Intel Mac 為 `x86_64`）：

- `chefer-vmlinuz-<arch>`、`chefer-initramfs-<arch>`（appliance）
- `chefer-gui-overlay-<arch>.sqfs`（僅 `--gui` 需要）
- `pasta-<arch>`（選用；`--gui` 的 bridge 出網要它，缺了會降級成 internal）

**這幾個本輪都沒改**，所以最快是抓 [latest release](https://github.com/TimLai666/chefer/releases/latest) 的 **apple-darwin kit**（`chefer_<ver>_<arch>-apple-darwin.tar.gz`，解開後有 `kit/`），指 `--kit-dir` 到那個 `kit/` 即可。

> ⚠️ **重點**：release 的 kit 也含一份 `guest-agent-<arch>`，但那是**舊版、不含本輪的 GUI 修復**（尤其 boot-race 的 `WLR_LIBINPUT_NO_DEVICES=1`）。vz-smoke 已改成**優先用 cross 從當前原始碼自建 guest-agent**、忽略 kit 的舊版。所以請務必裝 `cross`（否則會退回舊 guest-agent，vz GUI 可能踩到已修好的競態）。

想完全自建 kit（不抓 release）：
```bash
CHEFER_LINUX_REF=v6.6.32 bash scripts/build-appliance.sh --arch <arch> --out kit   # Docker
bash scripts/build-gui-overlay.sh --arch <arch> --out kit                          # Docker
# pasta 選用：bash scripts/build-pasta.sh --arch <arch> --out kit
```

---

## 跑驗證 + 怎麼判讀

### 核心路徑（非 GUI）
```bash
bash scripts/vz-smoke.sh --kit-dir kit
```
腳本會**自動斷言**（失敗會 `die` 並印 log 路徑）：
1. **VM 開機 + guest-agent + exit code 傳播**：跑一個 `exit 7` 的一次性服務 → 驗 console 出現 `CHEFER_GUEST_EXIT=` 且單檔以 **7** 退出。
2. **服務常駐 + TCP 埠轉發**：跑 httpd、驗 console 出現 `CHEFER_GUEST_IP=` 且 host `curl 127.0.0.1:18080` 通。

全綠代表 vz 的開機/virtiofs/guest-agent/埠轉發這條主線在真 Mac 上成立。

### GUI 路徑（手動檢核）
```bash
bash scripts/vz-smoke.sh --kit-dir kit --gui
```
會開一個 xclock 視窗（順帶用 bridge 出網 `apk add xclock`，驗 pasta NAT）。**請人工核對**：

- [ ] 出現以 app 名為標題的視窗、xclock 指針在動（`VZVirtualMachineView` 顯示）
- [ ] 滑鼠移動/點擊、鍵盤輸入進得了 guest（HID）
- [ ] host 複製文字 → guest 內可貼上；guest 複製 → host 可貼上（剪貼簿；圖片可用小畫家/預覽 PNG 測）
- [ ] **macOS 14+：拖拉視窗改變大小 → guest 畫面是否真的換解析度**（見下方「特別想觀察的點」）
- [ ] 關窗 → app 乾淨結束、行程退出（關窗＝app 結束）

---

## 本輪的發現，接手時要知道的

1. **boot-race 已修，vz 應同樣受惠**：WHP 實機發現 GUI 約 1/3 間歇開機失敗，根因是 **cage/wlroots 的 libinput backend 在「還沒有輸入裝置」時 abort**（virtio-input 節點由 udev 非同步建立、cage 贏競態就見 0 裝置）。修法 `WLR_LIBINPUT_NO_DEVICES=1` 在 `guest-agent/src/gui.rs`，**vz GUI 走同一條 gui.rs**，所以理論上一起修好了——但**這點值得在 Mac 上確認**（vz 的輸入裝置由 VZ 提供，時序可能不同）。

2. **動態解析度——vz 可能比 WHP 更好，值得實測**：WHP 實測發現 **cage 不會因線上模式變更 re-modeset**，所以 WHP 是「視窗縮放、guest 維持原生解析度」。但 **vz 走的是 `VZVirtualMachineView.automaticallyReconfiguresDisplay`（macOS 14+）**，由 VZ 直接對 guest 的 virtio-gpu 重設組態——這條路**可能真的會 re-modeset**（不像 WHP 只能縮放）。**請在 Mac 上拖拉視窗、看 guest 畫面是「換解析度」還是「拉伸」**，結論回填 DESIGN/README。

3. **guest-agent 在 VM 內的 stderr 預設看不到**（console 在 `quiet` 下只顯示高優先序 kmsg）。要在 guest 內除錯：寫 `/dev/kmsg`、優先序 `< 4`、且**整行一次 `write_all`**（每個 write() 是一筆 kmsg 記錄，`writeln!` 會拆開、訊息本體那筆沒前綴會被 `quiet` 擋掉）。gui.rs 的 `note()` 已用這招，且會把 cage 啟動失敗的 stderr 轉到 kmsg——**若 GUI 開不起來，先看 console 的 `[guest-agent] gui: cage: ...` 行**，那就是 wlroots 的真正錯誤。

4. **`CHEFER_GUEST_IP` 標記**：vz 用它做 host→guest 埠轉發與剪貼簿直連。若埠轉發沒動作，先確認 console 有這行。

---

## 驗證通過後要更新的地方

把「待實機」標記改成已驗證：

- `README.md` 平台表（第 ~29–39 行）macOS 欄的 🔧 → ✅、以及 GUI 欄「real-Mac VZ pending」字樣。
- `README.md` roadmap（第 ~266–267 行）「macOS VZ boot validated」「GUI apps on macOS」兩項 `[ ]`/`[~]` → `[x]`。
- `docs/DESIGN.md` §6 macOS 分期 ③④ 的「待實體 Mac 驗證」字樣、以及動態解析度那段（依實測是 re-modeset 還是縮放修正）。
- 若一切成立，可考慮把 `vz.rs` 的 `CHEFER_VZ_EXPERIMENTAL` 閘門放寬（例如驗證過的 macOS 版本才預設可用）——這屬**改變預設對外行為**，動之前先確認。

---

## 可能踩到的雷

- **arch 必須相符**：VZ 是虛擬化非模擬，Apple Silicon → `linux/arm64` guest、Intel Mac → `linux/amd64`。腳本以 `uname -m` 自動選，kit 要有對應 arch 的 appliance/overlay/guest-agent。
- **未簽章 binary 首次執行**：macOS 可能擋 `chefer` / helper，System Settings → Privacy & Security 放行。
- **沒裝 cross**：腳本會退回 kit 的舊 guest-agent 並**大聲警告**——那樣驗的不是當前碼（可能重現已修好的 boot-race）。要驗當前 main 請務必 `cargo install cross`（需 Docker）。
- **GHA 上驗不了**：別期待 CI 覆蓋這條；CI 只保證編譯+簽章+與 WHP 已驗證實作鏡像一致。

---

## 相關檔案

- `scripts/vz-smoke.sh` — 這支一鍵驗證腳本
- `crates/vmm-backend/src/vz.rs`、`vz_util.rs` — vz 後端（Rust）
- `vz-helper/main.swift` — VZ 開機/GUI/剪貼簿 helper（Swift）
- `crates/guest-agent/src/gui.rs` — VM 內 GUI 環境（cage/udev/seatd，含 boot-race 修復與 kmsg 觀測）
- `docs/DESIGN.md` §6「macOS（vz）」「GUI」 — 契約與分期
