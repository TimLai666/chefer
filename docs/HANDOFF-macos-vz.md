# Handoff：在 Mac 上驗證 macOS vz 後端

> **✅ 狀態：驗證已完成（2026-07-11，Apple Silicon / macOS 26）。** 核心路徑（開機/exit code/TCP+UDP 埠轉發）、bridge 出網（pasta+DNS）、多服務/persist、GUI（顯示、剪貼簿文字雙向+PNG）全數通過；動態解析度實測為**兩段式**：開機時 guest 模式=視窗 Retina 像素尺寸（`CHEFER_VZ_GUI_SIZE` 生效），拖拉時 cage 不自行跟模式變更——**後續已由 guest-agent 的 resize watcher 補上 guest 側 re-modeset（全鏈實測通過，見「發現」第 2 點；HiDPI output scale 補上後已翻為 macOS 14+ 預設）**。結果已回填 README 與 DESIGN §6。實機揪出並修掉四顆 bug：extract 不還原 exec bit、release 的 aarch64 initramfs 內嵌 x86-64 busybox、runtime 埠代理與 vz relay 搶埠、容器缺 `/etc/resolv.conf`（bridge 出網通但 DNS 全掛）。仍待人工的互動項：實際打字點擊（HID）、拖拉縮放、關窗語意；兩個已知問題（剪貼簿連線偶發靜默 `.waiting`、對 runtime 單發 signal 會殘留 helper）見 DESIGN §6。以下原文保留供重跑參考，**過時處以「實測更正」標註**。

這份是給「拿到一台實體 Mac、要把 chefer 的 macOS（Virtualization.framework / vz）路徑收尾」的接手文件。程式碼側都寫完了，剩下的**只差在真 Mac 上跑一次驗證**——GitHub 的 macOS runner 是巢狀虛擬化、開不了 VZ guest，所以 CI 只能編譯+簽章，端到端一定要實機。

## TL;DR

```bash
# 1. 前置：Xcode CLT、Rust（含 <arch>-unknown-linux-musl target；免 cross/Docker）；備一份含 appliance/overlay 的 kit
# 2. 一鍵驗證（非 GUI 的核心路徑：開機 / exit code 傳播 / 埠轉發）
bash scripts/vz-smoke.sh --kit-dir <放 appliance 的資料夾>
# 3. 加 --gui 再驗顯示 / HID / 剪貼簿 / 動態解析度 / 關窗語意（會列手動檢核清單）
bash scripts/vz-smoke.sh --kit-dir <...> --gui
```

腳本自己會**從當前原始碼**建 `chefer-vz-helper`（swiftc + virtualization entitlement 簽章）、`chefer-cli`、`chefer-runtime`、以及 `guest-agent`（原生 cargo musl build，有 cross 則優先 cross），只從 `--kit-dir` 拿兩個「需要 Docker 才建得出」的 Linux 產物：**appliance kernel/initramfs** 與 **GUI overlay squashfs**（注意：≤ v0.4.0 release kit 的這兩樣不可用，見下方「實測更正」）。

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
| musl target（`rustup target add <arch>-unknown-linux-musl`） | 從原始碼建 `guest-agent`（純 Rust + rust-lld，**免 cross/Docker**） | 腳本會自動 `rustup target add`；有 cross 則優先用 cross |
| 一份 **kit**（appliance + overlay） | vz-smoke 從這裡拿 vmlinuz/initramfs/overlay | 見下節 |

### 取得 kit（appliance + gui-overlay）

腳本只需要 kit 裡這幾個檔（`<arch>` = Apple Silicon 為 `aarch64`、Intel Mac 為 `x86_64`）：

- `chefer-vmlinuz-<arch>`、`chefer-initramfs-<arch>`（appliance）
- `chefer-gui-overlay-<arch>.sqfs`（僅 `--gui` 需要；動態解析度要 **cage ≥0.2.0**＝新版 `build-gui-overlay.sh`（Alpine 3.22）建的）
- `pasta-<arch>`（選用；`--gui` 的 bridge 出網要它，缺了會降級成 internal）

~~**這幾個本輪都沒改**，所以最快是抓 [latest release](https://github.com/TimLai666/chefer/releases/latest) 的 **apple-darwin kit**~~ **實測更正（2026-07）**：≤ v0.4.0 的 release kit **不能**直接用——(a) `chefer-initramfs-aarch64` 內嵌 x86-64 busybox（交叉建置 bug，guest 直接 ENOEXEC panic；已修 `scripts/appliance/build-inside-container.sh`）；(b) kernel 缺 `SQUASHFS`/`BLK_DEV_LOOP`（GUI overlay 掛不起來）；(c) overlay 是舊 `.tar.zst` 格式（現行 `.sqfs`）。請用 Docker（OrbStack 亦可）從當前原始碼重建 appliance + overlay（指令見下）；`pasta-<arch>` 可沿用 release kit。

> ⚠️ **重點**：release 的 kit 也含一份 `guest-agent-<arch>`，但那是**舊版、不含本輪的 GUI 修復**（尤其 boot-race 的 `WLR_LIBINPUT_NO_DEVICES=1`）。vz-smoke 會**優先從當前原始碼自建 guest-agent**、忽略 kit 的舊版。**實測更正（2026-07）：不需要 cross/Docker**——guest-agent 是純 Rust + rust-lld（`.cargo/config.toml` 已設 linker），`rustup target add <arch>-unknown-linux-musl` 後用原生 `cargo build --target <arch>-unknown-linux-musl --release -p guest-agent` 即可（vz-smoke 已支援此原生 fallback）。

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

2. **動態解析度——guest 側 re-modeset 已補上（實機 e2e 通過）**：實測證實 cage 不會自行跟隨線上模式變更（vz 的 `automaticallyReconfiguresDisplay` 只解決 host→guest 這半段）。現由 guest-agent 的 **resize watcher（`resize.rs`）**補上：drm uevent → `GETCONNECTOR` re-probe → `wlr-randr --custom-mode`（wlr-output-management）要 cage 換模式，vz 與 WHP 共用。**前提：GUI overlay 需建自新版 `build-gui-overlay.sh`（Alpine 3.22：cage 0.2.0 + wlr-randr）**，vz-smoke 有 Docker 會自動現建。**真動態解析度已翻為 macOS 14+ 預設（2026-07-12）**，`CHEFER_VZ_DYNAMIC_RESOLUTION=0` 退回 view 縮放保底（DESIGN §6 ④）；配 `CHEFER_VZ_GUI_TEST_RESIZE=20:1100x700` 可免拖拉自動驗證（console 應出現 `gui: resize: Virtual-1 -> 2200x1400`，guest 內 `xdpyinfo` dimensions 跟著變——已在 Apple Silicon 實測通過）。**WHP 實機回歸待跑**（WHP 無 opt-in 閘，拖拉即觸發）。**HiDPI output scale 已接上（實機機器可驗項通過，2026-07-12）**：helper 於動態解析度模式把開機當下螢幕的 backingScaleFactor 以 `chefer.gui_scale=` 附進 kernel cmdline，watcher 開機補設一次 output scale、之後每次換模式的同一次 apply 一併帶 `--scale`。實機 e2e（xdpyinfo probe 容器 + `CHEFER_VZ_GUI_TEST_RESIZE=30:1100x700`）：console 出現 `gui: resize: Virtual-1 output scale -> 2`，`xdpyinfo` 開機即回報**邏輯**尺寸 1280x800（實體 2560x1600）；縮放後 `gui: resize: Virtual-1 -> 2200x1400 (scale 2)`、`xdpyinfo` 跟隨 1100x700。**人工目測（游標/UI 尺寸、座標映射（xeyes）、Xwayland 畫質；拖拉細黑邊確認為 debounce 前暫態）2026-07-12 亦通過 → 已翻為 macOS 14+ 預設**，`CHEFER_VZ_DYNAMIC_RESOLUTION=0` 退回 view 縮放保底（回歸清單見 `vz-smoke.sh --gui`）。

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
