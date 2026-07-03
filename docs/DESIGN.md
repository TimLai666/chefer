# Chefer 設計契約（v1）

本文件是所有 crate 共同遵守的「單一事實來源」。任何跨 crate 的格式、API、行為變更都必須先改這份文件。

## 0. 全域原則

- **依賴版本**：新增依賴一律用 `cargo add`（由 crates.io 解析），**禁止手寫猜測版本號**。專案自身版本號不主動調整。
- **可編譯性**：整個 workspace 必須在 Windows / Linux / macOS 三平台都能 `cargo build`。平台限定邏輯用 `#[cfg(target_os = "...")]` 隔離，其他平台給出可編譯的 stub（回傳明確錯誤）。
- **語言**：所有給使用者看的輸出一律使用英文（CLI 的 println/eprintln、錯誤訊息、clap help、驗證訊息、init scaffold、examples 註解）；程式碼註解、`docs/DESIGN.md`、commit message、PR 描述、`#[cfg(test)]` 斷言訊息一律使用繁體中文。錯誤訊息需可行動（說明缺什麼、怎麼補）。
- **安全**：所有 tar 解包都必須做路徑安全檢查（拒絕絕對路徑、`..`、Windows 前綴）；symlink/hardlink 目標必須限制在解壓根目錄內。
- 編輯既有 crate 時維持 `edition = "2024"`。

## 1. 整體架構與資料流

```
建置期（開發者機器，任何平台）:
  appcipe.yml
    → appcipe-spec   (純解析 + 驗證)
    → appcipe-normalize (路徑絕對化、舊欄位遷移、套預設; 提供 load() 一站式入口)
    → chefer-pack    (解析 image tar → 以「層」形式寫入 bundle + manifest.json)
    → chefer-assembler (runtime 執行檔 + zstd(tar(bundle)) + footer → 單一執行檔)

執行期（終端使用者機器）:
  單一執行檔 = chefer-runtime
    → 讀自身 footer、驗證 sha256、解壓 bundle 至 temp
    → 解析 manifest、解析 data dir、old_names 遷移
    → 啟動埠代理（host≠guest 時）
    → vmm-backend 選擇後端執行:
        Linux   → namespaces 後端（in-process 呼叫 guest-agent lib）
        Windows → WSL2 後端（chefer 專用 distro 內跑 bundle 內嵌的 musl guest-agent）
        macOS   → Virtualization.framework 後端（v1 為骨架，回報明確未支援訊息）
    → guest-agent: 由層組 rootfs（whiteout）、bind mounts、依 depends_on 順序啟動服務、fail_fast 監控
```

**關鍵決策：bundle 內不存「已解開的 rootfs」，而是存「原始層（layer）的 zstd 壓縮 tar」。**
理由：Windows host 無法忠實保存 Linux rootfs（symlink、權限、xattr、大小寫敏感）。rootfs 一律在 Linux 環境（namespaces / WSL2 / VM）內組裝。

## 2. Bundle 佈局 v1

```
bundle/
├─ manifest.json                  # 唯一執行協定（schema 見 §3）
├─ appcipe.yml                    # （選）原始設定回寫
├─ agents/                        # （選）內嵌 guest-agent 與 VM host helper
│  ├─ guest-agent-x86_64          # 對應 linux/amd64
│  ├─ guest-agent-aarch64         # 對應 linux/arm64
│  ├─ chefer-vz-helper-<arch>     # （選）macOS vz host helper
│  └─ chefer-whp-helper-<arch>.exe # （選）Windows WHP host helper
├─ vm/                            # （選）VM 目標內嵌的 Linux micro-VM appliance
│  ├─ chefer-vmlinuz-<arch>       # 預編 Linux kernel
│  └─ chefer-initramfs-<arch>     # 最小 initramfs（/init 掛 virtiofs 後 exec guest-agent）
└─ services/
   └─ <svc>/
      └─ layers/
         ├─ 0000-<diffid前12碼>.tar.zst   # 依序的 diff 層；內容 = zstd(未壓縮 layer tar)
         ├─ 0001-<diffid前12碼>.tar.zst
         └─ ...
```

- 層檔名：`{序號:04}-{diff_id去掉"sha256:"後取前12碼}.tar.zst`。
- `agents/` 規則：建置 Windows / macOS 目標的單檔時**必須**內嵌對應 guest 架構的 agent（缺少時 build 報錯並說明如何取得 kit）；Linux 目標可省略（Linux 後端 in-process 執行）。
- `vm/` 規則：建置 VM 目標的單檔時內嵌 Linux appliance（kernel+initramfs）到 `vm/`，並把對應 host helper 內嵌到 `agents/`：macOS `vz` 使用 `chefer-vz-helper-<arch>`，Windows `whp` 使用 `chefer-whp-helper-<arch>.exe`。兩者皆 best-effort——kit 缺少時 build 以警告略過（產物仍可組裝，僅執行時由對應後端回報不可用；macOS 另可用 `CHEFER_VZ_HELPER` 指向自建 helper），不阻斷建置。release kit 兩種 darwin/windows host 架構的 appliance 與 helper 皆出貨（vz helper 以 virtualization entitlement ad-hoc 簽章；正式對外散布尚需 Developer ID 簽章 + notarization）。

## 3. manifest.json schema（chefer-bundle::Manifest）

所有欄位由 `crates/chefer-bundle` 的 serde 型別定義，**其他 crate 一律 import 該型別，不得自行手刻 JSON**。

```json
{
  "format_version": 1,
  "app": {
    "name": "StudioPro",
    "app_version": "2.3.1",
    "spec_version": "0.1",
    "old_names": ["Studio"],
    "data_dir_override": "D:/Apps/StudioPro",
    "crash": "fail_fast",
    "network": "bridge",
    "console": "auto",
    "generated_at_utc": "2026-06-11T00:00:00Z",
    "builder_version": "0.1.0"
  },
  "services": [
    {
      "name": "db",
      "platform": "linux/amd64",
      "layers": [
        { "rel_path": "services/db/layers/0000-abcdef123456.tar.zst",
          "diff_id": "sha256:abcdef…(64hex)", "size": 12345 }
      ],
      "image_config": {
        "entrypoint": ["docker-entrypoint.sh"],
        "cmd": ["postgres"],
        "env": ["PATH=/usr/local/sbin:…", "PGDATA=/var/lib/postgresql/data"],
        "working_dir": "/",
        "user": "postgres",
        "exposed_ports": ["5432/tcp"]
      },
      "cmd_override": { "shell": "echo hi" },
      "env": { "POSTGRES_PASSWORD": "pw" },
      "workdir_override": null,
      "persist_path": "/var/lib/postgresql/data",
      "ports": [ { "host": 5432, "guest": 5432, "proto": "tcp" } ],
      "mounts": [ { "host": "C:/data", "guest": "/mnt/data", "read_only": false } ],
      "interface_mode": "none",
      "depends_on": [],
      "healthcheck": {
        "test": { "argv": ["redis-cli", "ping"] },
        "interval_ms": 2000,
        "timeout_ms": 5000,
        "retries": 10,
        "start_period_ms": 0
      }
    }
  ]
}
```

- `services` 依 name 排序（輸出確定性）。
- `cmd_override`：serde untagged enum `CmdSpec`：`{"shell": "..."}` 或 `{"argv": ["..."]}`。
  - 語意（沿用 Docker）：override **只取代 image 的 Cmd，不取代 Entrypoint**。
  - shell 形式由 guest-agent 轉成 `["/bin/sh", "-c", s]`（此時忽略 entrypoint？否——仍接在 entrypoint 後）。
  - 有效命令 = `entrypoint + (cmd_override 或 image_config.cmd)`；兩者皆空 → 啟動失敗並報錯。
- `env` 合併順序：image_config.env（基底）→ appcipe env 覆蓋。`PATH` 若最終為空，guest-agent 補預設 `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`。
- `interface_mode`: `"gui" | "terminal" | "both" | "none"`。
- `app.network`: `"shared" | "internal" | "bridge"`（預設 `bridge`；見「網路隔離」節）。舊 bundle 無此欄位 → 反序列化為預設。
- `app.console`: `"auto" | "shown" | "hidden"`（預設 `auto`；見「主控台顯示」節）。舊 bundle 無此欄位 → 反序列化為 `auto`。

## 4. Footer v1（單檔尾端 80 bytes）

由 `chefer-bundle::footer` 統一讀寫（runtime 與 assembler 都用它）：

| 位移 | 長度 | 內容 |
|---|---|---|
| 0 | 8 | magic `b"CHEFER\0\0"` |
| 8 | 1 | version = 1 |
| 9 | 1 | flags（bit0 = payload 為 zstd 壓縮的 tar）|
| 10 | 6 | 保留（0）|
| 16 | 8 | u64 LE：payload 在檔案中的 offset |
| 24 | 8 | u64 LE：payload 長度（bytes）|
| 32 | 32 | payload（壓縮後 bytes）的 SHA-256 |
| 64 | 16 | 保留（0）|

- payload = `zstd(tar(bundle/))`，tar 內路徑以 `bundle/` 為根（即解壓後得到 `<dest>/bundle/manifest.json`）。
- sha256 驗證必須**串流**計算（不可一次將整個 payload 讀進記憶體；app 可能數 GB）。

## 5. Runtime kit（建置期資源）

`chefer build` 需要兩類預編譯二進位，統稱 kit：

- `chefer-runtime-<target-triple>[.exe]`（例：`chefer-runtime-x86_64-pc-windows-msvc.exe`）
- `guest-agent-<arch>`（musl 靜態，arch ∈ `x86_64`,`aarch64`）

搜尋順序：CLI `--kit-dir` > 環境變數 `CHEFER_KIT_DIR` > `<chefer exe 所在目錄>/kit/` > `<chefer exe 所在目錄>/`。
另外接受「host target 的 runtime」以不帶 triple 的檔名存在（`chefer-runtime[.exe]`）。
找不到時報錯，訊息需列出已搜尋的路徑與期望檔名，並提示可從 GitHub Releases 下載或以 `cargo build -p chefer-runtime` 自建。

## 6. 各 crate 公開 API 契約

### chefer-bundle（新 crate，lib）
- `Manifest`/`AppMeta`/`ServiceEntry`/`LayerRef`/`ImageConfig`/`CmdSpec`/`PortSpec`/`MountSpec`/`InterfaceMode`/`CrashPolicy` serde 型別。
- `Manifest::load(path) / save(path)`。
- `footer::{Footer, FOOTER_LEN, FLAG_ZSTD, MAGIC}`：`Footer::read_from_file(path)`、`Footer::write_bytes(&self) -> [u8; 80]`、欄位同 §4。
- `PortSpec::parse("host:guest[/proto]")`（預設 tcp；驗證 1..=65535）。
- `MountSpec::parse("<host>:<guest>")`：從**右往左**切一次 `:`（相容 `C:\`）。
- layout 輔助：`manifest_path(bundle_dir)`、`service_layers_dir(bundle_dir, svc)`、`agents_dir(bundle_dir)`、`guest_agent_name(arch)`、`pasta_name(arch)`、`vz_helper_name(arch)`、`whp_helper_name(arch)`、`platform_to_arch(platform)`、`layer_file_name(idx, diff_id)`。
- `topo_sort(services) -> Result<Vec<&ServiceEntry>>`（depends_on 拓撲排序；偵測循環）。
- `kit` 模組（**kit 探索統一在此**，pack/assembler/cli 共用）：`default_kit_dirs()`、`find_runtime(kit_dirs, target, allow_plain)`、`find_guest_agent(kit_dirs, arch)`、`find_pasta(kit_dirs, arch)`、`find_appliance(kit_dirs, arch)`、`find_vz_helper(kit_dirs, arch)`、`find_whp_helper(kit_dirs, arch)`、`runtime_file_name(target)`、`not_found_help(...)`。

### appcipe-spec（lib）
- 維持 `from_file/from_str`（內容=解析+驗證；**路徑正規化移到 normalize**。spec **不得**依賴 normalize——正式入口是 `appcipe_normalize::load`，CLI/pack 一律走那裡）。
- 舊欄位 `crash_policy` → `crash` 以 `#[serde(alias = "crash_policy")]` 處理（屬解析層）。
- 驗證規則（`validate()`，回傳所有錯誤的彙整而非只有第一個）：
  - `version` 必須是 `"0.1"`。
  - `name`：`[A-Za-z][A-Za-z0-9_-]*`，長度 ≤ 64。
  - service 名稱：`[a-z][a-z0-9_]*`，長度 ≤ 32。
  - ports：可被 `PortSpec::parse` 接受；同一 app 內 host port 不得重複（不分協定，從嚴）。
  - mounts：可被 `MountSpec::parse` 接受。
  - `persist_path` 必須以 `/` 開頭。
  - `depends_on`：必須指向存在的 service；不得有循環；不得指向自己。
  - `healthcheck`（選填，見「健康檢查」節）：有則 `test` 非空；`interval`/`timeout` 須能解析且 > 0；`retries` ≥ 1；`start_period` 能解析且 ≥ 0。`test` 接受字串或字串陣列（`CMD`/`CMD-SHELL` 前綴比照 Docker）。
  - env key：`[A-Za-z_][A-Za-z0-9_]*`。
  - `image.source` 支援 `tar`（本機 docker save / OCI archive）與 `image`（建置時從 registry 拉取）；`dockerfile` 仍回報「尚未支援」。`source: image` 的 `file` 是 registry reference，**必須釘版**：明確且非 `latest` 的 tag，或 `@<algo>:<hex>` digest（`check_image_reference`）；未標記/`latest` 一律拒絕（可重現性）。
- **簡寫 `image: <字串>` 的判別（`appcipe-normalize::looks_like_image_ref`）**：非路徑徵兆（不以 `.`/`/`/`\`/`~` 開頭、非 Windows 磁碟代號、非 `.tar`/`.tgz`/`.tar.gz`、且檔案不存在）且帶 `@digest` 或「最後一段含 `:tag`」→ 轉成 `source: image`（compose 風格）；否則維持 tar 路徑。tag 合法性仍由 `validate()` 把關。
- `AppCipe` 衍生 `Clone`。

### appcipe-normalize（改為 lib）
- `pub fn normalize(app: &mut AppCipe, base: &Path) -> anyhow::Result<()>`：
  - host 路徑絕對化（image.file、mounts 左半、data_dir）——沿用既有 rsplitn 邏輯。
  - 舊欄位遷移（目前：YAML 層級 `crash_policy` → `crash`；以 serde alias 或前置轉換實作，列入測試）。
- `pub fn load(path: &Path) -> anyhow::Result<AppCipe>`：讀檔 → 解析 → normalize → validate。CLI 與 pack 一律走這裡。

### chefer-pack（lib）
- `pub struct PackOptions { pub out_dir: PathBuf, pub clean: bool, pub write_original_yml: bool, pub kit_dirs: Vec<PathBuf>, pub target_triples: Vec<String>, pub require_agents: bool, pub zstd_level: i32, pub builder_version: String, pub local_run: bool }`（`local_run`：是否為 `chefer run` 的本機立即執行，決定 mount host 路徑檢查嚴格度，見下方行為節）
- `pub struct PackResult { pub bundle_dir: PathBuf, pub manifest: chefer_bundle::Manifest }`
- `pub fn pack(app: &AppCipe, opts: &PackOptions) -> anyhow::Result<PackResult>`
- 行為：
  0. 取得每個 service 的 image，收斂成同一個 `ResolvedImage`（config + 各層 blob 檔），來源三擇一：
     - **`source: tar`**：image tar 先安全解壓到暫存目錄再解析（OCI blobs 需隨機存取）。
     - **`source: image`**：以 `oci-client`（rustls）依 `platform` 從 registry 拉 manifest+config+layers（`chefer-pack::registry`，pack 同步流程內 `block_on` 一個 tokio runtime；匿名，僅公開 image），layer blob 寫進暫存目錄。之後與 tar 共用同一條 repack。各層 blob 的解壓/雜湊/重壓全程串流。
     - **`source: dockerfile`**：在**打包機**上以既有的 container builder 建置（見下方「Dockerfile build」），`save` 成 docker-archive tar 後，**完全併入 `source: tar` 的解析路徑**。
  1. `source: tar` 時**自動偵測格式**（看內容不看副檔名）：
     - docker-archive：根目錄有 `manifest.json`（JSON 陣列，元素含 `Config`/`Layers`）。
     - oci-archive：根目錄有 `index.json` + `blobs/`（layout 可含 `oci-layout`）。
  2. multi-arch：oci index 依 service `platform` 挑 manifest；找不到該平台→報錯列出可用平台。docker-archive 多筆 entry 時亦同（依 config 的 os/architecture）。
  3. 層資料可能是 gzip(`+gzip`)、zstd(`+zstd`) 或未壓縮——一律**解到未壓縮 tar 再以 zstd 重壓**寫入 bundle；同時驗證/計算 diff_id（未壓縮 tar 的 sha256）與 config 的 `rootfs.diff_ids` 一致。
  4. 從 image config 抽出 `ImageConfig`（Entrypoint/Cmd/Env/WorkingDir/User/ExposedPorts）。
  5. 寫 manifest.json（用 chefer-bundle 型別）。
  6. 從 kit 複製 guest-agent 到 `agents/`（依 services 用到的 arch 去重；`require_agents=false` 時缺少僅警告）。
  7. 若 `target_triples` 含 VM target，從 kit best-effort 複製對應架構的 appliance 到 `vm/`，並複製對應 host helper 到 `agents/`：macOS target 需要 `chefer-vz-helper-<arch>`，Windows target 需要 `chefer-whp-helper-<arch>.exe`；缺少時只警告、不阻斷建置，產物在執行時由對應後端回報明確不可用原因。
- **mount host 路徑檢查（便利性防呆，非正確性需求）**：打包前對各 service bind mount 的 host 半邊在**打包機**上做存在性檢查。但 manifest 只存該絕對路徑字串，guest-agent 於執行期會在**真正的執行 host** 重新檢查（見 guest-agent 服務啟動節：「bind mounts（host 路徑不存在→啟動前報錯）」），故此 build 期檢查只在 **打包機 == 執行機** 時才有意義。
  - 真正能確定「打包機 == 執行機」的只有 `chefer run` 在**非 VM 後端**時（本機建置、本機立即執行）。`chefer build` 產出的單檔本來就是要散布到別台機器執行，在打包機檢查路徑等於檢查錯的機器；VM 後端（macOS vz／appliance）的 host 是 guest VM，同樣 host≠build-host（bind 的 host 半邊常是 guest 路徑如 `/mnt/data/...`）。
  - 因此 `PackOptions.local_run` 區分兩條路：**只有 `local_run == true` 且 build 不含 VM（darwin）目標時，缺路徑為 fail-fast 錯誤**；其餘（`chefer build`、或任何含 darwin 目標的 build）一律降級為**警告**，交由執行期在真正的 host 重新檢查。CLI：`chefer run` 設 `local_run=true`、`chefer build` 設 `false`。`scripts/qemu-e2e.sh` 走 `chefer build`（已是寬鬆），故不需在 runner 上預建 guest 路徑。
- tar 讀取一律串流，不把整層讀進記憶體。

### chefer-assembler（lib + bin）
- `pub struct AssembleOptions { pub zstd_level: i32 }`
- `pub fn assemble(runtime_bin: &Path, bundle_dir: &Path, out_path: &Path, opts: &AssembleOptions) -> anyhow::Result<AssembleReport>`
  - 複製 runtime → out_path，串流 append `zstd(tar(bundle/))`，邊寫邊算 sha256，最後寫 footer。
  - out_path 在非 Windows 目標要 `chmod +x`（在 Unix host 上）。
- kit 探索一律用 `chefer_bundle::kit`（不要在 assembler 重複實作）。
- bin 介面：`chefer-assembler --runtime <path> --bundle <dir> --out <path> [--zstd-level N]`（除錯用；正式入口是 chefer-cli）。

### chefer-runtime（bin）
- 流程：footer → 串流驗證 sha256 → 解壓 temp（`--extract-dir`、`--keep-tmp` 真正生效）→ `Manifest::load` → data dir 解析與 old_names 遷移 → 啟動埠代理 → `vmm_backend::run_app` → 傳遞 exit code → 清理。
- data dir 解析：`app.data_dir_override` > 平台預設：
  - Windows: `%LOCALAPPDATA%\{name}`
  - macOS: `~/Library/Application Support/{name}`
  - Linux: `$XDG_DATA_HOME/{name}` 或 `~/.local/share/{name}`
- old_names 遷移：新目錄不存在時，依序找同層舊名目錄，第一個存在者 rename 為新名。
- 埠代理：
  - **TCP**：對每個 `host != guest` 的 PortSpec 啟一條 thread：`listen 127.0.0.1:host → connect 127.0.0.1:guest` 雙向轉送（後端候選先 127.0.0.1 後 [::1]）。`host == guest` 在 Linux 不代理（共享 netns）、在 Windows 加 best-effort IPv4 補橋。
  - **UDP**：採 per-client session table relay（每個來源位址各開一條 upstream socket 並各起回程 thread；非「只記最後來源」）。
    - Linux：`listen 127.0.0.1:host → 127.0.0.1:guest`（共享 netns）。
    - Windows（wsl2）/ macOS（vz）：因 WSL2 的 `wslrelay` 不轉 UDP（VZ NAT 同理不自動轉送），改由後端取得 VM 的對外 IPv4，host 端 relay `127.0.0.1:host → <vm_ip>:guest`（含 `host == guest`，UDP 在 Windows 不會自然生效，故一律 relay）；VM 內由 guest-agent 起 `<vm_ip>:guest → 127.0.0.1:guest` 的橋接，補上「服務只綁 loopback」的情形（服務綁 0.0.0.0 時直接命中 `<vm_ip>:guest`，橋接以 EADDRINUSE 略過）。此 UDP 的 host 端 relay 因需 VM IP，**由 wsl2/vz 後端啟動**，不在 runtime 的跨平台 `start_port_proxies`（後者在 Windows 略過 UDP）。
- Ctrl-C / SIGTERM：終止後端與子行程、清 temp、以 130 退出。

### vmm-backend（改為 lib）
```rust
pub enum Availability { Available, Unavailable(String) }
pub struct RunOptions { pub keep_tmp: bool }
pub struct AppRunContext<'a> {
    pub bundle_dir: &'a std::path::Path,
    pub data_dir:   &'a std::path::Path,
    pub manifest:   &'a chefer_bundle::Manifest,
    pub opts: RunOptions,
}
pub trait ExecBackend {
    fn name(&self) -> &'static str;
    fn availability(&self, ctx: &AppRunContext) -> Availability;
    fn run(&self, ctx: &AppRunContext) -> anyhow::Result<i32>; // app 整體 exit code
}
pub fn backends() -> Vec<Box<dyn ExecBackend>>;        // 依平台排序
pub fn run_app(ctx: &AppRunContext) -> anyhow::Result<i32>; // 取第一個 Available 的後端執行；全部不可用→彙整原因報錯
```
- **Linux**（`namespaces`）：可用性 = `/proc/self/ns/user` 存在且允許 unprivileged userns（讀 `/proc/sys/kernel/unprivileged_userns_clone` 若存在）。執行 = in-process 呼叫 `guest_agent::run_bundle`。
- **Windows**（`wsl2`）：可用性 = `wsl.exe --status` 成功且支援 WSL2。執行：
  1. agent 二進位 = `bundle/agents/guest-agent-<arch>`（缺→明確錯誤）。
  2. distro 名 = `chefer-rt-<agent sha256 前 8 碼>`；runtime 會先在 `%LOCALAPPDATA%\chefer\wsl\runs\<distro>\` 建立本次執行的 marker，避免同一個 distro 被多個 packaged app 共用時誤清。distro 不存在時：產生最小 rootfs tar（標準 FHS 目錄 + `/bin/guest-agent` + `/etc/wsl.conf`（automount enabled + `options="metadata"`）+ `/etc/passwd` + **`/bin/mount`、`/bin/umount` → guest-agent 的 symlink** + **`/tmp/.X11-unix` → `/mnt/wslg/.X11-unix` symlink**，讓 WSLg 存在時 X11 client 可找到 socket），`wsl --import <distro> %LOCALAPPDATA%\chefer\wsl\<distro> <tar> --version 2`。
     **重要（實測根因）**：WSL init 啟動 distro 時會執行 distro 內的 `/bin/mount` 來掛載 drvfs（呼叫形如 `mount -i -t 9p C:\ /mnt/c -o msize=65536,trans=fd,rfdno=5,wfdno=5,cache=mmap,noatime,aname=drvfs;...`，9p 連線經繼承的 fd）。缺 mount 時 automount 與 interop 全滅。guest-agent 以 busybox 風格 applet（argv[0] 派發，`applets.rs`）提供 mount/umount。
  3. 以 `wsl -d <distro> --user root --exec /bin/guest-agent run --bundle <wsl路徑> --data <wsl路徑> --cache /var/lib/chefer/cache` 執行；rootfs 快取一律放 distro 內 ext4（/mnt/c 的 drvfs 上 symlink/hardlink/權限不可靠且 I/O 慢）；Windows 路徑以純函式轉換（`C:\foo` → `/mnt/c/foo`；UNC/相對路徑報錯），不依賴 wslpath；stdio 直通；exit code 透傳。guest-agent 結束後 runtime 移除自己的 marker；若沒有其他仍活著的 marker，會 best-effort 執行 `wsl --terminate <distro>` + `wsl --unregister <distro>` 並清掉 `%LOCALAPPDATA%\chefer\wsl\<distro>`。清理失敗只印 warning，不改變 app exit code；需要保留 distro 除錯或量測時可設 `CHEFER_KEEP_WSL_DISTRO=1`。
  4. 注意命令注入：所有外部參數都走 argv 陣列（`std::process::Command` 個別 arg），絕不組 shell 字串。
  5. **網路（實測）**：WSL2 的 localhost 轉送（wslrelay）只綁 IPv6 `[::1]`；runtime TCP 埠代理後端因此「先試 127.0.0.1、再退 [::1]」，且對 `host == guest` 的 TCP 埠在 Windows 上加 best-effort 的 IPv4 補橋（127.0.0.1:port → [::1]:port）。
  6. **UDP 埠映射（已解決，實測）**：wslrelay 不轉 UDP，故 wsl2 後端在啟動 guest-agent 前先以 `wsl -d <distro> --exec /bin/guest-agent vmip` 取得 VM eth0 的 IPv4，對每個 UDP PortSpec（含 `host == guest`）於 host 端起 `127.0.0.1:host → <vm_ip>:guest` 的 session relay；並以 `--udp-bridge` 旗標叫 guest-agent 在 VM 內補起 `<vm_ip>:guest → 127.0.0.1:guest` 橋接（涵蓋服務只綁 loopback 的情形；服務綁 0.0.0.0 則直接命中、橋接以 EADDRINUSE 略過）。VM IP 每次啟動現查（NAT 模式下會變）。已知殘留：服務若綁 0.0.0.0 且 bind 時機晚於 VM 內橋接（橋接已設寬限期降低機率），可能與橋接搶埠——屬罕見且會以明確 bind 錯誤呈現、可重試。
  7. **免 WSL 的 `whp` 後端**：以 **Windows Hypervisor Platform（WHP）** 開機 bundle 內附的 Linux micro-VM appliance（與 macOS `vz` 共用同一 kernel/initramfs/guest-agent），作為 `wsl2` 的替代後端，移除對 WSL2 的依賴（仍需硬體虛擬化 + WHP 功能）。對完全無虛擬化的機器，另可選擇性 bundle 軟體模擬（QEMU/TCG）作為最終備援（可跑但慢）。backend 抽象（`ExecBackend`）已納入 `whp`，Windows 後端排序為 `wsl2` → `whp`；`whp` 以 `WinHvPlatform.dll` + `WHvGetCapability(HypervisorPresent)` 做 host preflight，並在有 bundle context 時檢查 `vm/chefer-vmlinuz-<arch>`、`vm/chefer-initramfs-<arch>`、`agents/chefer-whp-helper-<arch>.exe` 是否存在——三者皆備且 hypervisor 可用時回傳 `Available`，由 runtime 的 `run()` spawn helper 執行完整開機，stdout 逐行解析 `CHEFER_GUEST_IP=<ipv4>` / `CHEFER_GUEST_EXIT=<code>` 標記。helper CLI contract：`chefer-whp-helper --kernel <p> --initramfs <p> --cmdline <s> --bundle-dir <p> --data-dir <p> --cpus <n> --memory-mib <n> [--timeout <secs>] [--forward-tcp <host:guest>]... [--forward-udp <host:guest>]...`（`--forward-tcp`/`--forward-udp` 可重複，宣告 host→guest TCP/UDP 埠轉發；見下 virtio-net 說明，WHP 的埠轉發 listener 由 helper 自身持有）。
     **WHP 專屬 kernel 參數（已實測確認）**：`nolapic`（WHP LAPIC emulation 不完整，改由 host 直接注入 timer interrupt）、`lpj=1000000`（跳過 calibration busy-wait）、`notsc clocksource=jiffies`（TSC 在 minimal VM 不可靠）。此外 initramfs 內的 init 必須為 **非 PIE 靜態 ELF**（static non-PIE，`ET_EXEC`；PIE 的 `ET_DYN` 在無 dynamic linker 的 minimal VM 會 segfault），且 initramfs 必須包含 `/dev/console`（char device major=5, minor=1）供 kernel 開啟 init 的 stdio fd 0/1/2。
     **Guest→host exit code 通道（`/dev/kmsg`）**：guest init 結束前將 `CHEFER_GUEST_EXIT=<code>` 寫入 **`/dev/kmsg`**（非 stdout），因 user-space 寫 `/dev/console`（tty 路徑）需 serial IRQ4 驅動 TX，而 WHP 的最小 8250 模擬不含 IRQ 觸發——`/dev/kmsg` 走 kernel printk polled I/O 路徑，可直接出現在 serial console。
    **`--preflight` 模式**（`chefer-whp-helper --preflight [--cpus <n>]`）：動態載入 `WinHvPlatform.dll` → 走一輪 WHP device model 基礎生命週期，不開機、不需 appliance 或 bundle 路徑：
    1. partition 建立與設定：`WHvCreatePartition` → `WHvSetPartitionProperty(ProcessorCount)` → `WHvSetupPartition`；
    2. GPA 記憶體映射：`VirtualAlloc` 配置一頁（4 KiB）零值記憶體 → `WHvMapGpaRange` 映射到 GPA 0（驗證 host→guest 記憶體管理路徑）；
    3. vCPU 建立：`WHvCreateVirtualProcessor` 建一顆 VP（index 0）並立即 `WHvDeleteVirtualProcessor` 銷毀（驗證處理器管理路徑）；
    4. 清理：`WHvUnmapGpaRange` → `VirtualFree` → `WHvDeletePartition`。
    成功 exit 0 並印 OK 行到 stdout，失敗 exit 69 並印 HRESULT。runtime 的 `run()` 與 `chefer doctor` 會自動呼叫 `--preflight` 報告 WHP API 功能性。
    **完整開機模式（boot shim）**（已實作並在 Windows 上實測通過——Alpine virt 6.6 kernel 完整開機到 init）：
    1. **bzImage 解析**（`bzimage.rs`，跨平台）：解析 Linux boot protocol header（0x1F1–0x268）、驗證 magic / protocol version / LOADED_HIGH、計算 kernel payload offset。
    2. **Guest 記憶體佈局**：GDT 0x1000、stack 0x7000、boot_params 0x8000、cmdline 0x20000、kernel 0x100000（1MB）、initramfs page-aligned 接 kernel 後方、LAPIC dummy page 0xFEE00000。
    3. **vCPU 初始狀態**：CR0=0x00000031（PE+NE+ET，**無 PG**——paging 必須關閉，由 kernel startup_32 自行啟用 PAE/long mode）、CR3=0、CR4=0、CS/DS/ES/FS/GS/SS=flat 32-bit（`__BOOT_CS=0x10`, `__BOOT_DS=0x18`）、TR=32-bit TSS busy（selector 0x28）、LDTR=not present、RSI=boot_params、RIP=code32_start、RSP=0x7000、RFLAGS=0x02。
    4. **Run loop**：`WHvRunVirtualProcessor` → 依 exit reason 處理：
       - **IoPortAccess**：serial（COM1 0x3F8-0x3FF，8250/16550 UART 模擬，`serial.rs`）、CMOS（0x70-0x71）、PIC（0x20-0x21, 0xA0-0xA1，`pic.rs`，完整 ICW 初始化序列/IMR/EOI）、PIT（0x40-0x43，counter latch/mode/readback）、port 0x61（System Control Port B，bit 4/5 交替——TSC calibration 需要看到 bit 5 變化才能完成，否則無限迴圈）、keyboard controller（0x60/0x64，回 0）。
       - **Halt / Canceled / None**：先刷 serial、檢查 `CHEFER_GUEST_EXIT=<code>` 標記與 "Kernel panic" 字串；若均未偵測到則檢查 RFLAGS.IF（bit 9）：IF=0 + HLT 代表 kernel 完成 `cli; hlt` shutdown loop（`reboot(POWER_OFF)` 或 `halt`），偵測到 "System halted" / "Power down" → clean exit、否則報錯中止；IF=1 則注入 timer interrupt 繼續執行。
       - **MemoryAccess / UnrecoverableException / InvalidVpState**：報錯中止（含 GPA 與 RIP）。
    5. **Serial console**（`serial.rs`，跨平台）：完整 DLAB switching、LSR 回報 transmitter ready、scratch register、CMOS battery-OK 回傳。Guest 輸出即時串流到 stdout。
    6. **Timer interrupt 注入**：背景執行緒每 10ms 呼叫 `WHvCancelRunVirtualProcessor` 打斷 run loop；主執行緒檢查 RFLAGS.IF（bit 9），若中斷已啟用（IDT 已設好）則透過 `WHvSetVirtualProcessorRegisters(WHvRegisterPendingInterruption=0x80000000)` 直接注入 vector 0x20——繞過 WHP LAPIC（此 Windows 組建的 WHP LAPIC emulation 不攔截 MMIO），改以 CPU register 直接遞送。LAPIC 在 GPA 0xFEE00000 配置一頁 dummy memory，回傳基本 version/spurious 值，讓 kernel APIC 初始化不 fault。
    7. **PIC/PIT 模擬**（`pic.rs`，跨平台可測）：8259A PIC 完整 ICW1-4 初始化序列、IMR 讀寫、non-specific/specific EOI、IRR/ISR 切換讀取。8254 PIT channel 0/2 的 counter latch、lo/hi 交替讀寫、mode command。port 0x61 的 refresh/channel-2 output bit 交替——解決 Linux early boot TSC calibration busy-wait。
    8. **virtio-mmio 裝置模型（virtio-blk 已實機達成；目標＝讓真 bundle 在無 WSL 的 Windows 一鍵跑起來）**：**狀態（2026-06）：virtio-blk + virtio-net 路徑皆已在實體 Windows + WHP 完整跑通**——`chefer build` 的真 bundle 經 `CHEFER_BACKEND=whp` 執行，guest 從 `/dev/vda`(bundle ro)/`/dev/vdb`(data) 讀到內容、guest-agent 在 WHP VM 內組 alpine rootfs 起 `interface_mode: none` 服務、乾淨回 `CHEFER_GUEST_EXIT=0`；且 host→guest TCP 埠轉發實測可達（redis bundle，host `redis PING` 得 `+PONG`）。boot shim 在 §4 的最小裝置（serial/PIC/PIT）外，再補出與 vz 等效（vz 靠 VZ 內建 virtiofs/virtio-net/virtio-console）的裝置模型。選 **virtio-over-MMIO** 而非 virtio-pci：WHP 不模擬 PCI host bridge，且 x86 上 virtio-mmio 可純靠 kernel cmdline `virtio_mmio.device=<size>@<base>:<irq>` 靜態註冊，免 PCI 列舉與 device tree。契約：
       - **GPA 佈局**：在 kernel/initramfs/LAPIC 之外另闢 virtio-mmio 暫存器區（每裝置 0x200 bytes，例如 base `0xD000_0000` 起依序 +0x200），各配一個 PIC IRQ（如 5/6/7，避開既用線）。
       - **MMIO transport 與指令解碼（關鍵決策）**：WHP 的 `EXIT_MEM_ACCESS` 只給 GPA + access type、**不給指令語意**（不像 PIO 的 `IoPortAccess` 已帶 port/方向/size/rax）。故 **用 WHP 內建指令模擬器 `WinHvEmulation.dll`**（`WHvEmulatorCreateEmulator` + `WHvEmulatorTryMmioEmulation`／`TryIoEmulation`，透過 Get/SetVirtualProcessorRegisters、TranslateGvaPage、Memory/IoPort callback 解碼存取），**不自寫 x86 decoder**。callback 內依 GPA 落在哪個 virtio-mmio 視窗 dispatch 給對應裝置。
       - **virtqueue**：split virtqueue（desc／avail／used ring 在 guest RAM，host 以 GPA 讀寫）；feature 協商至少 `VIRTIO_F_VERSION_1`。used ring 更新後經 PIC 注入該裝置 IRQ（沿用既有 inject 機制）。ring 解析與 register state machine 為跨平台純邏輯，進 CI 單元測試。
       - **裝置清單**：① **virtio-blk（bundle, ro）**＋② **virtio-blk（data, rw）**——取代 vz 的 virtiofs。bundle/data 各以 **sector(512) 對齊的 tar image** 當 backing（`virtio::image::pack_dir`／`unpack_image`，純 Rust、跨平台可產生，免在 Windows host 備 mke2fs/mksquashfs）；guest 端 busybox `tar` 展開到 tmpfs；data 於關機時把 image 解回 host 持久化。**已知取捨**：tar 需展開佔 guest RAM——對 data（小）合適，bundle（大）若 RAM 吃緊，後續可換唯讀 squashfs image。③ **virtio-net**——host→guest 埠轉發，host 端走 **純 Rust user-mode TCP/IP（smoltcp）**：helper 內以 smoltcp 當 guest 的 gateway（gateway `10.0.2.2`、guest 靜態 `10.0.2.15/24`，與 appliance init 約定），net 裝置兩個 queue（0=rx、1=tx）在 base `0xD000_0200` / IRQ 6。**埠轉發歸屬（WHP 專屬，與 vz 不同）**：smoltcp 的 guest IP 是 helper process 內的虛擬位址、host kernel 無路由可達，故 host→guest 轉發的 listener **必須由 helper 自身持有**——但 helper 並非綁使用者的 host 埠，而是按 WSL2 wslrelay 慣例把**每個 guest 埠暴露在 host `[::1]:<guest_port>`**（`--forward-tcp <listen>:<guest>` → `NetBackend::add_forward` 在 helper 內 bind `[::1]:listen`，再以 smoltcp TCP socket 橋接到 `guest_ip:guest`）。host≠guest 的對外 remap 仍交給 **chefer-runtime 既有的埠代理**（`proxy.rs`：bind `127.0.0.1:host` → 轉 `127.0.0.1`/`[::1]:guest`）——故 vmm-backend 只把要暴露的 guest 埠（去重）以 `<guest>:<guest>` 傳給 helper，**不可**像 vz 那樣直連 guest IP relay、也不重綁使用者 host 埠（會與 runtime proxy 撞埠）。綁 `[::1]` 而非 `127.0.0.1` 是為了同時相容 runtime 的 host==guest Windows 補橋（它佔 `127.0.0.1:guest` 並轉 `[::1]:guest`）。**UDP 同理**（`--forward-udp` → `add_udp_forward` 綁 `[::1]:listen` UDP，per-client smoltcp UDP socket 以 unique local port demux guest 回程），guest 端由 guest-agent 的 `start_vm_udp_bridges`（eth0→loopback UDP 橋接）承接。guest 主動對外（outbound NAT）已實作（見下方 M7）：helper 的 `drain_tx` 把外部 dst 的 UDP/TCP 分流到 NAT 引擎（per-flow host socket），guest 內 `shared` 服務直走 eth0、`bridge` 服務經 pasta 到 eth0，皆可出網。④ console 沿用 ttyS0 + `/dev/kmsg` exit channel（§4 不變），暫不引入 virtio-console。
       - **appliance 相容**：需要能在 WHP 環境開機的 appliance。kernel config 加 `CONFIG_VIRTIO_MMIO`／`_BLK`／`_NET`／`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`；cmdline 追加各 `virtio_mmio.device=...`。**init 改為自適應**（偵測 `/dev/hvc0` → vz 路徑掛 virtiofs；否則 WHP 路徑掛 `/dev/vda`(bundle ro)＋`/dev/vdb`(data rw)、console 用 ttyS0），維持「whp 與 vz 共用一份 appliance」。init 另負責 `bridge` 出網（pasta）的環境前提：開機早期 `switch_root` 到 tmpfs 根（rootfs 上 `pivot_root` EINVAL）、確保 `/dev/net/tun` 存在且 0666；kernel config 含 `CONFIG_TUN`。
       - **分階段里程碑（每個可獨立驗證；✅=已實機達成）**：✅ M1 transport（WHvEmulator 接線 + virtqueue）→ ✅ M2 virtio-blk(bundle ro)（guest 從 `/dev/vda` 讀 bundle 解 tar）→ ✅ M3 virtio-blk(data rw) 關機回寫（**實機達成**：helper 第二顆 virtio-blk vdb（base `0xD000_0400`/IRQ 7）以 `pack_dir(data_dir)` 補零到容量上限為 backing；guest 開機 `dd /dev/vdb | tar -x`、關機 `tar data_dir | dd of=/dev/vdb`，helper 關機 `unpack_image(vdb)→data_dir`；本機 WHP 實測 persist counter 跨重啟 1→2→3。**限制**：只在 guest 乾淨關機（服務全退出）時回寫，長駐 server 被 kill/timeout 不回寫；vdb 容量固定 `CHEFER_WHP_DATA_MIB`（預設 256MiB）；tar 解壓不刪除 host 端已移除的檔）→ ✅ M4 virtio-net + host→guest 埠轉發（**實機達成**：net 裝置接上 run loop／MMIO 模擬器、smoltcp user-mode gateway、helper 綁 `[::1]:guest`、guest 靜態 IP `10.0.2.15`、guest-agent eth0→loopback TCP 橋接；本機 WHP 實測 redis bundle，host `redis PING` 經整條鏈得 `+PONG`）→ ✅ M5 appliance 自適應 init（偵測 /dev/hvc0 → vz virtiofs；否則 WHP `dd /dev/vda | tar -x`）+ kernel config + build → ✅ M6 端到端真 bundle（實機 `CHEFER_GUEST_EXIT=0`，`interface_mode: none` 服務）。**剩餘**：guest 主動對外 outbound NAT（🚧 分階段：✅ **M7-a 出網 UDP（實機達成）**——`whp-helper/src/virtio/nat.rs` 解析 guest IPv4/UDP frame + 合成回程 frame；net_backend NAT 引擎（per-flow host UDP socket）+ `drain_tx` 分流（外部 dst → NAT、其餘 → smoltcp）+ 回程注入 guest rx；本機 WHP 實測 `network: shared` 服務 `nslookup example.com 1.1.1.1` 經整條 NAT 鏈解析成功（`CHEFER_GUEST_EXIT=0`）。✅ **M7-b 出網 TCP（實機達成）**——drain_tx 對外部 dst 的 TCP 仍交 smoltcp，但 SYN 先 `nat_tcp_syn` 預註冊：把 dst IP 動態加進 iface `ip_addrs`（X/32；`.cargo/config.toml` 設 `SMOLTCP_IFACE_MAX_ADDR_COUNT=64` 提高容量 + idle dst LRU 驅逐）、建 smoltcp listen socket(dst,port)、**背景執行緒** connect host `TcpStream`（不阻塞 VM loop）；`tcp_nat_pump` 完成連線後雙向橋接 smoltcp socket ↔ host stream；本機 WHP 實測 `network: shared` 服務 `wget http://example.com`（DNS UDP NAT 解析 + TCP NAT 連 104.20.x:80）成功（`CHEFER_GUEST_EXIT=0`）。✅ **M7-c bridge 模式出網（實機達成，預設模式全通）**——不需取代 pasta：與其他後端同一條 pasta 路徑（app netns → pasta tap → guest root netns socket → eth0 → helper NAT）。實機打通靠三個 guest 端修正：① appliance kernel 補 `CONFIG_TUN`（x86_64 defconfig 不含，pasta 無 tap 可建）；② appliance init 補 `/dev/net/tun` 節點 0666（devtmpfs 對 misc 裝置預設 0600，pasta 以非特權 uid 開不了）並於開機早期 `switch_root` 到 tmpfs 根（initramfs 的 rootfs 上 `pivot_root` 一律 EINVAL，pasta 自我沙箱會直接退出——此問題 vz 同 appliance 也會中，一併修掉）；③ guest-agent `start_pasta` 對 exec `EACCES` 補救（Windows host 打包無法記錄 unix 執行位 → bundle 內 pasta 是 0644；複製到 tmp 補 0755 重試）。本機 WHP 實測預設 `bridge` app：alpine 服務 `wget http://example.com`（DNS UDP + HTTP TCP 經 pasta→NAT 鏈）`CHEFER_GUEST_EXIT=0`；redis bundle 宣告埠 `+PONG`、未宣告埠拒絕。）。剩餘：GUI 顯示通道（virtio-gpu，另一條線）。
       - **驗證邊界（誠實）**：同 vz——**GitHub runner 開不了 WHP**，transport／virtqueue 純邏輯進 CI 單元測試，真開 VM 的 e2e 只能在實機（Windows + 硬體虛擬化 + WHP 功能）手動跑。前置：`CHEFER_BACKEND=whp` 後端覆寫開關（已實作）讓有 WSL 的機器能選到 whp。
- **macOS**（`vz`，Apple Virtualization.framework）：macOS 沒有現成 Linux，必須自行開一台輕量 Linux micro-VM 來跑容器。設計如下（**狀態：契約已定 + 跨平台純邏輯已實作並測試；Linux appliance 建置與 QEMU E2E 已納入 scripts/CI；開機改採內附 Swift helper（見下），其 swiftc 編譯與 Rust 後端 compile-check 已納入 CI，VM 真正開機仍待在實體 Mac 上驗證**——在實機驗證通過前 `availability()` 預設 `Unavailable`，需 `CHEFER_VZ_EXPERIMENTAL=1` 才啟用，不偽稱可執行）。
  - **Appliance（kit 內附預編 kernel）**：kit 與 macOS 目標的 bundle 內附一份精簡 Linux 開機組合：
    - `vm/chefer-vmlinuz-<arch>`：預編 Linux kernel（含 virtio-blk/virtiofs/virtio-net/overlayfs）。
    - `vm/chefer-initramfs-<arch>`：最小 initramfs，其 `/init` 掛載 virtiofs 共享的 bundle 與 data 後，`exec` 內附的 guest-agent `run --bundle /mnt/bundle --data /mnt/data`。
    - arch 對應同 `layout::platform_to_arch`（x86_64 / aarch64）；Apple Silicon 上以 arm64 guest 為主。
  - **開機機制 — 內附 Swift helper（`chefer-vz-helper-<arch>`）**：實際驅動 Virtualization.framework 的是一支以 **Swift（Apple 第一方 VZ API）** 寫的小 helper，與 guest-agent/pasta/appliance 同屬 kit 內附的協力執行檔，打包 macOS 目標時嵌進 bundle（`agents/chefer-vz-helper-<arch>`，host macho、host 架構）。選 Swift 而非 in-process objc2 綁定：VZ 的 Swift API 文件完整、正確性高、可獨立編譯/簽章；Rust 後端只負責 spawn helper、串接其 stdout、解析 console 標記、做 host 端埠轉發——這條 Rust 路徑在任何平台都可單元測試。helper 與 chefer-runtime 以 **CLI 介面 + stdout 標記**鬆耦合（見下）。
  - **VM 組態（helper 內，`VZVirtualMachineConfiguration`）**：`VZLinuxBootLoader(kernelURL = vmlinuz, initialRamdiskURL = initramfs, commandLine = "console=hvc0 quiet ip=dhcp panic=-1 ...")`；CPU/記憶體由 helper 參數帶入（CPU = min(host, 4)、RAM ≥512MiB，可由 env 覆寫）；share 用 `VZVirtioFileSystemDeviceConfiguration`（virtiofs）把 bundle（唯讀）與 data dir（讀寫）掛進 guest；`VZVirtioConsoleDeviceSerialPortConfiguration` + `VZFileHandleSerialPortAttachment` 把 guest 的 hvc0 接到 helper 的 stdout（guest console → helper stdout → chefer-runtime 讀取解析）；`VZNATNetworkDeviceConfiguration` + entropy/记忆体气球等預設裝置。helper CLI：`chefer-vz-helper --kernel <p> --initramfs <p> --cmdline <s> --bundle-dir <p> --data-dir <p> --cpus <n> --memory-mib <n>`；開機成功→0、VZ 設定/開機錯誤→非 0（guest 整體 exit code 仍由 console 的 `CHEFER_GUEST_EXIT` 標記回傳）。
  - **Console 標記契約（host 解析 guest 狀態的唯一管道）**：appliance `init` 在 hvc0 印出兩個標記，host 端以子字串比對解析（`vz_util::parse_guest_exit_code` / `parse_guest_ip`，取最後一個、容忍前綴）：
    - `CHEFER_GUEST_EXIT=<code>`：guest-agent 結束後印出，隨即關機 → host 以此作為整個 app 的 exit code。
    - `CHEFER_GUEST_IP=<ipv4>`：開機掛載後、跑 app 前印出 guest eth0 的對外 IPv4，供 host 規劃 host→guest 埠轉發。
  - **網路 / 埠映射**：`VZNATNetworkDeviceAttachment` 給 guest NAT IP（host 在同一 NAT 子網可直接連到 guest）；但 host→guest **不自動轉送**，故所有宣告的 ports（TCP 與 UDP）都由 vz 後端在 host 端 relay `127.0.0.1:host → <guest_ip>:guest`（`guest_ip` 取自上面的 console 標記；轉發清單由 `vz_util::forward_ports` 規劃）。UDP 另以 `--udp-bridge` 叫 guest-agent 在 VM 內補 eth0→loopback 橋接（appliance init 已帶此旗標，guest 側就緒；**macOS host 端 relay 與 vz 開機 shim 同屬待實機驗證項**）。
  - **程式碼簽章 / entitlement**：呼叫 Virtualization.framework 的程序是 **`chefer-vz-helper`**（非 chefer-runtime 本身），故 helper 在 macOS 上**必須**以 `com.apple.security.virtualization` entitlement 簽章，否則建立 VM 會在執行期失敗。release 流程建好 helper 後即以此 entitlement 簽章再嵌進 kit/bundle（macho 的簽章隨檔案複製保留，執行期解壓後仍有效）；自行於 Mac 測試時可 ad-hoc 簽：`codesign --entitlements vz-helper.entitlements -s - chefer-vz-helper-<arch>`。
  - **實驗性開關（誠實回報）**：在實體 Mac 驗證通過前，`availability()` 預設維持 `Unavailable`；提供 `CHEFER_VZ_EXPERIMENTAL=1` 讓使用者在自己的 Mac 上明確選擇啟用 vz 後端做驗證（訊息會說明這是未驗證路徑）。驗證通過後才移除此 gate、預設可用。
  - **純邏輯（跨平台可測，`vz_util.rs` 內）**：appliance 檔在 bundle 內的查找（`layout::vm_dir` / `kernel_name` / `initramfs_name`）、kernel command line 組裝、VM 資源（CPU/RAM）計算、guest 路徑映射、錯誤訊息——皆為純函式並有單元測試（在 Windows/Linux 上即可跑）。
  - **Swift helper（`vz-helper/`，僅 macOS 編譯）**：以 swiftc 編成 `chefer-vz-helper-<arch>`，驅動上述組態並開機。只能在實體 Mac 驗證 VM 真的開得起來（GHA macOS runner 為巢狀虛擬化、開不了 VZ guest，CI 僅 swiftc compile-check helper、cargo compile-check Rust 後端）。Rust 後端找 helper 的順序：bundle 內 `agents/chefer-vz-helper-<arch>` ＞ `CHEFER_VZ_HELPER` env（供實機驗證時直接指向自建 helper，免整條 release 管線）。
  - **Appliance 建置**：`vm/chefer-vmlinuz-*` 與 `vm/chefer-initramfs-*` 由獨立的 Linux 建置流程產生，並可在 **Linux + QEMU**（同樣 virtio 開機）上做 E2E 驗證，與 macOS 無關——這是讓 vz 後端可信的關鍵前置，且不需 Mac。
  - **架構對應（Intel vs Apple Silicon）**：VZ 是虛擬化非模擬，**guest 架構必須 == host 架構**。Apple Silicon → arm64 guest（需 `aarch64` appliance/agent 與 `platform: linux/arm64` 的 image）；Intel Mac → x86_64 guest（需 `x86_64` 與 `linux/amd64`）。kit 兩架構皆出貨、macOS 目標分 `x86_64-apple-darwin` / `aarch64-apple-darwin`；但**打包出來 app 的 image `platform:` 必須對上該 Mac 的 CPU**（不跨架構）。Apple Silicon 另有 **`VZLinuxRosettaDirectoryShare`**：可讓 arm64 guest 以 Rosetta 跑 x86_64 Linux 執行檔（未來可據此讓 `linux/amd64` image 在 Apple Silicon 上執行）；Intel 無此項。

  - **GUI（規劃；macOS 顯示 Linux app 視窗）— 採「virtio-gpu + 內嵌 kiosk compositor」路線**：

    macOS 無內建 X11/Wayland、且 app 跑在 micro-VM 內，無法沿用 Linux/WSL 的「傳 unix socket 給既有顯示伺服器」做法。採自包路線（零安裝、符合單檔精神）：

    - **Guest（appliance）**：kernel 開 `CONFIG_DRM_VIRTIO_GPU`（+ DRM/evdev）；initramfs 內嵌極小 kiosk Wayland compositor **`cage`** + Mesa（llvmpipe 軟算繪，或 virtio-gpu 加速）+ **Xwayland**（讓 X11 app 也能跑）。guest-agent 對 `interface_mode: gui/both` 的服務改以 `cage -- <服務命令>` 啟動（設好 `WAYLAND_DISPLAY`），算繪到 virtio-gpu scanout。`cage` 的「全螢幕單一 app」模型恰好對上 chefer「一個 app 至多一個介面服務」。
    - **Host（vz 後端，`#[cfg(target_os = "macos")]`）**：VM 組態加 `VZVirtioGraphicsDeviceConfiguration`（一個 scanout）+ USB 鍵盤/指標裝置；開一個 AppKit 視窗承載 `VZVirtualMachineView` 綁該 VM → 顯示 guest framebuffer，VZ 自動把鍵鼠 HID 轉進 guest。代表 macOS 版 chefer 在 gui 模式時要成為「有 NSApplication 事件迴圈的 GUI 程式」；非 gui 維持 headless。
    - **生命週期 / 輸入**：`cage` 內 app 結束 → 沿用 guest-agent 既有「介面服務結束＝收掉 app」→ VM 關、視窗關。鍵鼠走 VZ→USB HID→evdev→cage，免額外處理。
    - **剪貼簿（host↔guest，vz 與 whp 共用設計）**：**不走 vsock**（whp 得再寫一個 virtio-vsock 裝置模型，成本高）——改走**既有網路通道**：guest-agent 起剪貼簿同步服務（guest 側以 wl-clipboard 對接 cage 的 Wayland 剪貼簿），host 側經該後端既有的 host→guest 通道連入（vz：直連 guest IP；whp：helper 的 `[::1]` 轉發），雙向同步。安全：localhost TCP 任何本機程序可連 → 以 **kernel cmdline 帶入的隨機 token** 握手，未帶 token 的連線一律拒絕。首版同步 UTF-8 文字；圖片/大 payload 後續。
    - **分期**：① vz 後端先點亮（非 GUI）→ ② appliance 加 virtio-gpu + cage + Xwayland，先在 **QEMU** 驗 GUI app 能算繪 → ③ 真 Mac 接 `VZVirtualMachineView` + HID → ④ 剪貼簿/縮放打磨。全程依賴實體 Mac 才能完成 ③ 之後。
    - **替代（不採用）**：X11 → 要求使用者裝 **XQuartz**、X11 經 vsock 轉發——較快但破壞零安裝、過 VM 邊界延遲高、XQuartz 老舊，故不採。

  - **GUI overlay 打包契約（vz 與 whp 共用；規劃）**：cage + Xwayland + Mesa(llvmpipe) 及其依賴閉包太肥（估數十 MB），**不放進基礎 initramfs**——做成獨立 kit 產物 **`chefer-gui-overlay-<arch>`**（rootfs subtree 的 `tar.zst`，由 `scripts/build-gui-overlay.sh` 於容器內自 Alpine 套件收集，release workflow 建置、`verify-release-kit` 檢查）。
    - **嵌入規則**：app 有任一 `interface_mode: gui/both` 服務 **且** target 為 Windows 或 macOS（存在 VM 後端可能）→ chefer-pack 把 overlay 嵌入 bundle `vm/gui-overlay-<arch>.tar.zst`；純 server app 與 Linux target **不揹此體積**。Windows target 就算最後跑在 wsl2（用不到 overlay）也要嵌——build 時無從得知執行時會落在哪個後端。kit 缺 overlay → build 印明確警告（GUI 在 WHP/vz 將降級並附補救方式），不擋 build。
    - **kit 探索**：統一加在 `chefer_bundle::kit::find_gui_overlay(kit_dirs, arch)`，比照 guest-agent/pasta，不另起爐灶。`chefer upgrade` 整包換 kit → overlay 不會與 CLI 漂移；`inspect` 的 `vm/` 清單自然列出。
    - **消費端**：appliance init 見 bundle `vm/` 有 overlay → 解壓到 tmpfs 根（在既有 switch_root 之後）；guest-agent 對 gui/both 服務以 `cage -- <cmd>` 包裝。舊 bundle / 無 overlay 而有 gui 服務 → WHP/vz 印可行動錯誤（說明需以含 GUI overlay 的 kit 重新打包），不得無聲黑屏。

  - **WHP GUI（M8，規劃）— host 側自建顯示與輸入（guest 側與 vz 完全共用上述 overlay/cage 路線）**：vz 有 `VZVirtualMachineView` 免費送顯示+HID，WHP 全要自己來。**2D only**（不做 virgl/GPU 加速——WHP 路線本就 CPU-only，cage 用 llvmpipe 軟算繪；host 只做 framebuffer 搬運與顯示）；指標採 **virtio-input tablet（絕對座標）** 避免滑鼠捕捉問題。
    - ✅ **M8-a guest 側（QEMU 驗證達成，與 vz 共用）**：kernel 加 `CONFIG_DRM`/`CONFIG_DRM_VIRTIO_GPU`/`CONFIG_INPUT_EVDEV`/`CONFIG_VIRTIO_INPUT`；`scripts/build-gui-overlay.sh`（Alpine `apk --root` 閉包收集，x86_64 實測 209MB → zstd 56MB）；guest-agent `gui.rs`：init 設 `CHEFER_VM_GUI=1` 且 app 有 gui/both 服務時，以 ruzstd+tar 解 overlay 到根、起 udevd/seatd/cage、等 `wayland-0` socket、設 `DISPLAY`/`WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`（exec 既有 GUI socket bind 邏輯直接沿用；缺 overlay 依契約硬錯誤）。QEMU（KVM + virtio-gpu/keyboard/tablet + 螢幕截圖）實測：debian xclock（X11 經 Xwayland）全螢幕算繪於 scanout。**實測發現（缺一即敗）**：① Alpine libseat 沒編 builtin backend → 必須起 `seatd`（`LIBSEAT_BACKEND=seatd`）；② Xwayland 對「同 uid、無 cookie」的本機連線也拒絕（xhost 亦連不上），而 wlroots 不帶 `-auth` 也無法傳額外參數 → overlay 內以 shim 讓 Xwayland 一律帶 `-ac`（安全邊界＝micro-VM 本身，VM 內只有本 app 的服務）；③ cage 需要一個 client 引數 → 給長眠 sleep 佔位、服務作為一般 client 連入；④ `WLR_RENDERER=pixman` 強制軟算繪（VM 無 GPU，避免 GLES 探測失敗）。release 管線已接：`gui-overlay` job（兩架構）→ 各 host kit → `verify-release-kit` 檢查。
    - **M8-b 裝置模型純邏輯（與 M8-a 並行）**：`whp-helper/src/virtio/gpu.rs`（control queue：`GET_DISPLAY_INFO`/`RESOURCE_CREATE_2D`/`RESOURCE_ATTACH_BACKING`/`SET_SCANOUT`/`TRANSFER_TO_HOST_2D`/`RESOURCE_FLUSH`；cursor queue 後續）＋ `virtio/input.rs`（evdev 事件封包、event/status queue）。單元測試鎖行為，沿用 M1-M7 的 mmio/queue 積木。
    - **M8-c host 視窗**：helper 內 Win32 視窗執行緒（`RESOURCE_FLUSH` 時 `StretchDIBits` blit scanout；`WM_KEYDOWN`/`WM_MOUSEMOVE` 等轉 virtio-input 事件；使用者關窗 = 介面服務結束語意 → 整個 app 收掉）。只有 bundle 含 gui/both 服務才建視窗，否則維持 headless。
    - **M8-d 實機接線**：兩個新 virtio-mmio 視窗（沿既有佈局續排：gpu `0xD000_0600`/IRQ 8、input `0xD000_0800`/IRQ 9，實作時以 boot loop 佈局為準）+ cmdline `virtio_mmio.device=...` + 實機 GUI demo。
    - **M8-e 剪貼簿**：依上方共用設計（網路通道 + token），host 側 Win32 clipboard API + `WM_CLIPBOARDUPDATE`。
    - **首版範圍**：鍵盤＋滑鼠＋固定解析度（1280×800，env 可覆寫）＋ UTF-8 文字剪貼簿；動態解析度/DPI/圖片剪貼簿後續。

### guest-agent（lib + bin；Linux 專屬邏輯 cfg 隔離）
- lib：`pub struct RunConfig { pub bundle_dir: PathBuf, pub data_dir: PathBuf, pub cache_dir: Option<PathBuf>, pub keep_rootfs: bool, pub udp_bridge: bool }`
  `pub fn run_bundle(cfg: &RunConfig) -> anyhow::Result<i32>`（非 Linux 回傳明確錯誤）。`udp_bridge=true`（VM 後端：wsl2/vz）時，於啟動服務前 spawn 一條寬限執行緒在 VM 內補起 `<vm_ip>:guest → 127.0.0.1:guest` 的 UDP 橋接（見埠代理節）；namespaces 後端傳 false（原生 Linux 直接共享 netns，不得綁 LAN IP）。
- bin：`guest-agent run --bundle <dir> --data <dir> [--cache <dir>] [--keep-rootfs] [--udp-bridge]`；`guest-agent vmip`（印出 VM 對外 IPv4，供後端查詢，無則非 0 退出）；另提供 `guest-agent assemble-rootfs`（除錯）。
- rootfs 組裝：
  - 目的地：`cache_dir`（預設 `<data_dir>/.rootfs-cache`）`/<svc>-<chain_hash12>/`，chain_hash = sha256(diff_id 以 `\n` 串接)。已存在且 sibling 標記 `<svc>-<chain_hash12>.complete` 存在 → 直接重用。
  - 依序解每層 zstd tar：**whiteout 處理**——`.wh.<name>` → 刪除對應項；`.wh..wh..opq` → 清空該目錄既有內容；其餘正常解（保留 symlink/hardlink/權限；路徑安全檢查同 §0）。**解壓平行化**：多層時以 worker pool 提前平行解壓（純 Rust `ruzstd` 解壓吃 CPU），主執行緒仍**嚴格依序套用**（保住 overlay/whiteout 語意），背壓視窗限制暫存量。
  - 解完寫完成標記。**標記檔置於 rootfs/層目錄外（同層 sibling `<dir>.complete`）**，不可落在會成為容器 `/` 的目錄裡——否則合併路徑（rootfs 被 bind 成 `/`）與 overlay 路徑（lowerdir 內容原樣透出）都會讓容器根目錄多出一個 `/.complete`。overlay 各 lowerdir（`<cache>/layers/<diff_id>`）同理寫 sibling `<diff_id>.complete`。
  - **overlayfs / lazy rootfs（僅 root 後端）**：真 root（WSL2 / macOS VM / native-root）且 kernel 支援 overlay（以子行程實測掛載確認）時，改走 overlay——各層解到**以 diff_id 命名的獨立唯讀 lowerdir**（`<cache>/layers/<diff_id>`，內容定址 → 跨服務/跨 image 共用去重、持久），OCI whiteout 轉成 overlay whiteout（`.wh.x` → 字元裝置 0:0；`.wh..wh..opq` → `trusted.overlay.opaque` xattr，皆需真 root）。exec 時於服務 mount ns 內掛 `overlay`（lowerdir=各層 top-first、upperdir/workdir=每次執行的可寫層）到 merged 掛載點再 pivot_root——免合併複製、掛載瞬間完成。**rootless 不能 mknod whiteout 裝置 → 退回上面的合併路徑**。可寫層 ephemeral（系統 temp 下 `chefer-overlay/<pid>/<svc>/`，結束清除）；lowerdir 持久共用。**upperdir 須放 temp（通常 /tmp）而非 data_dir**——overlay upperdir 對 fs 有要求（d_type/xattr），而 data_dir 在 VM 後端常是 virtiofs（缺這些 → mount EINVAL）；`overlay_supported()` 的實測掛載即用同一 temp，順帶驗證該處可當 upper。
- 服務啟動（Linux）：
  - 依 `topo_sort` 順序啟動；若服務有 `healthcheck`，等待 healthy 後才繼續啟動後續服務。
  - 每個服務：依執行身分決定 namespace——**以真實 root 執行（WSL2 distro、macOS VM、或原生 Linux 以 root 執行）時 `unshare(mount+pid)`、不開 user namespace**，服務直接以 root 跑，容器 entrypoint 的 `chown`/`gosu`/`setuid` 到服務專用 uid（官方 redis/postgres 的 999…）可直接成功；**非 root（原生 Linux rootless）才先 `unshare(user)` 寫映射、再 `unshare(mount+pid)`**——映射兩條路（`guest-agent/src/subid.rs`）：主路徑為 **`/etc/subuid`/`/etc/subgid` 委派**（host 有 `newuidmap`/`newgidmap` 且使用者有委派範圍時，supervisor 於 fork 前建握手管線，子行程 `unshare(NEWUSER)` 後由 supervisor 以 newuidmap/newgidmap 代寫範圍映射——`0 <uid> 1` + 容器 uid 1 起依序接上各委派範圍，同 rootless podman；代寫路徑不寫 setgroups deny，`gosu` 的 setgroups 才可用 → **chown/gosu image 在 rootless 亦可跑**，已以官方 redis 非 root 實測）；fallback 為自寫單一 uid 映射（`0 <uid> 1`，核心規定 unshare 後自寫只能映射單一 id，此時 chown 到其他 uid 的映像受限）。internal/bridge 模式服務加入 netns holder 的 user ns，委派套在 holder 上（`setup_app_netns`），服務自動繼承；root 後端的 nobody holder 維持自寫單一映射（無委派概念）→ 掛 `/proc` → bind `/dev/{null,zero,random,urandom,tty}`、`/dev/pts`、`/dev/shm`(tmpfs) → bind persist（`<data_dir>/data/<svc>` ↔ persist_path，host 端先 `create_dir_all`）→ bind mounts（host 路徑不存在→啟動前報錯）→ interface_mode 含 gui 時 bind `/tmp/.X11-unix` 與 `$XDG_RUNTIME_DIR/wayland-*` socket（存在且為 socket 才掛；WSLg 內若未提供 `XDG_RUNTIME_DIR` 但 `/mnt/wslg/runtime-dir` 存在，則以該目錄作為 Wayland fallback）並傳遞 `DISPLAY`/`XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`（`WAYLAND_DISPLAY` 僅沿用實際已掛 socket，否則取排序後第一個）→ `pivot_root` → `chdir(workdir)` → env 合併（§3）→ exec 有效命令。
  - 網路（預設 = `bridge`）：`bridge`/`internal` 會建立 app 專屬 netns，宣告的 port 透過跨 netns relay 對外，未宣告 port 不可從 host 連入；`shared` 則保留舊的共享 netns 行為。
  - **depends_on 健康檢查（wait-until-ready）**：服務可選 `healthcheck`（見「健康檢查」節）。啟動採**拓撲序、序列化**：spawn 一個服務後，若它有 `healthcheck` 就**輪詢到 healthy 才 spawn 下一個**；無 healthcheck 的服務 spawn 即視為 ready（沿用 v1 行為）。因 `depends_on` 驅動拓撲序，被依賴者必在依賴者之前 ready，故 `depends_on: [db]` 等同「等 db ready」（db 有 healthcheck=等 healthy；否則=等 spawn）。輪詢期間同時偵測該服務崩潰與 SHUTDOWN。**fail_fast**：healthcheck 在 `retries` 次內（扣除 `start_period` 寬限）未成功 → 視為 unhealthy → terminate_all 並回非零碼。（MVP 序列化啟動：一個 healthcheck 會擋住其後所有服務，含不相依者；之後可改成只擋實際 dependents 的並行版。）
  - 監控（fail_fast + 介面服務生命週期）：
    - 任一服務 exit ≠ 0 → 終止其餘全部（SIGTERM → 等 10s → SIGKILL，對中繼 pgid 與 pid-ns init 雙送）→ 回傳該 exit code。
    - **介面服務（interface_mode 含 gui/terminal/both）即使 exit 0 也視為「整個 app 結束」**（使用者關掉視窗/終端 = app 該收掉）→ 終止其餘、回 0。否則 GUI app 關窗後背景服務（如 db）會殘留、佔住埠、無法重啟。
    - 非介面（none）服務 exit 0 → 屬背景/一次性任務，其餘繼續跑；全部結束 → 0。
  - `interface_mode=terminal/both`：該服務 stdio 直通（v1：所有服務 stdout/stderr 都加 `[svc]` 前綴轉發；terminal 服務 stdin 直通——僅允許一個服務宣告 terminal/both，多個→驗證期報錯）。彙整輸出的去向（那個「共用主控台」要不要顯示）由 app 級 `console` 控制，見下方「主控台顯示」。
- musl 靜態建置：`cargo build -p guest-agent --target x86_64-unknown-linux-musl --release` 必須可行（避免依賴需要 cc 的 crate；zstd 解壓用純 Rust 的 ruzstd）。musl 目標的 linker 統一為 rust-lld（`.cargo/config.toml` 已設定，跨 host 一致）。

### 網路隔離（per-app netns）— 設計

AppCipe 新增 app 級欄位 **`network`**（appcipe-spec enum，serde rename 小寫；寫進 manifest `app`）：

三種模式，**目標預設是 `bridge`**（對齊 Docker：app 有自己的私有網路、可出網、但只有宣告的 port 對外）。所有模式下，**整個 app 跑在自己的一個 network namespace** 內，服務間仍以 `127.0.0.1` 互通；差別只在「對外/出網」的策略：

- **`bridge`（目標預設；對齊 Docker 預設 bridge 網路）**：per-app netns（內有 `lo`）+ **bundled `pasta`（passt，userspace、rootless、免 iptables/veth）** 提供**出網 NAT** + 只轉發**宣告的 inbound 埠** = Docker bridge 等價行為（私有網路、可上網、只開宣告的埠）。guest-agent 從 host netns 以 `pasta --config-net --foreground --netns /proc/<holder>/ns/net [--userns …]` 對 app netns 提供連線；inbound 走下方的跨 netns relay。`pasta` 靜態 musl 二進位隨 kit 出貨並嵌入 bundle 的 `agents/pasta-<arch>`（同 guest-agent）；guest-agent 找 pasta 的順序為 `CHEFER_PASTA` > `agents/pasta-<arch>` > 與 guest-agent 同目錄/`kit/` > PATH。**找不到或啟動失敗時不致命**——`bridge` 退化為 `internal`（只有 lo、無對外網路）並印明確訊息。
- **`internal`（對齊 Docker compose 的 `internal: true` = 無對外網路）**：同 `bridge` 但**不起 pasta** → app netns 只有 `lo` → **服務沒有對外網路（無 internet / DNS）**。適合「內部專用、不需出網」的服務（db、純內部 API）。是 `bridge` 砍掉出網的子集。
- **`shared`（legacy／相容現況）**：服務共用 host/VM 的 netns；`ports:` 直接生效。缺點：未宣告的 port 仍可從 host 連到（且 WSL2 wslrelay 會鏡射任何 loopback 埠）→ **不隔離**。沿用現有路徑（含 `--udp-bridge`）。保留給需要「app 直接看到 host 網路環境」的場景。

**inbound（宣告的 `ports:`）跨 netns relay**（`bridge`/`internal` 共用）：guest-agent 對每個宣告的 port 起一條 **跨 netns 的 userspace relay**——listener 留在 **parent netns**（distro/VM/原生 host 的 netns）上的 `guest` 埠，connector 在 **app netns** 連 `127.0.0.1:guest`，兩端以 fork 前建立的 **socketpair（fd 不受 netns 限制）** 串接——**不需 veth / CAP_NET_ADMIN / iptables**，rootless 也適用（`unshare(NEWNET)` 在 userns 內可行）。沿用 guest-agent 既有的 per-client session TCP/UDP relay 邏輯（`udp_bridge`），只是改成跨 netns。
- **未宣告的 port**：服務只在 app netns 的 `lo` 監聽 → parent netns 無對應 listener → host 連不到、**WSL2 wslrelay 也看不到（服務不在它監看的 netns）→ 真正不對外**。這正是隔離的核心。
- host 端機制不變：chefer-runtime 的 TCP/UDP 代理、WSL2 的 localhost forwarding / UDP VM-IP relay 一律打到 **parent netns 上的 relay listener**（與 `shared` 時打服務本身位置等效）。

平台對應:
- **原生 Linux（namespaces）**:app netns 於 guest-agent in-process 建立;relay 跨「host netns ↔ app netns」;`bridge` 的 pasta 在 app netns 內跑。
- **Windows（wsl2）**:app netns 在 distro 內;relay 跨「distro netns ↔ app netns」;對外仍靠既有 host 代理 + wslrelay/UDP bridge 打到 distro netns 的 listener。附帶好處:**消除 wslrelay 對未宣告埠的鏡射洩漏**。
- **macOS（vz）**:VM 已與 host 隔離;VM 內同原生 Linux 作法。

實作分期（**已全部完成**）:**P1** 原生 Linux 完整做 `bridge`（app netns + 跨 netns inbound relay + pasta 出網）與 `internal`（同路徑不起 pasta）+ 單元/E2E 驗證（CI e2e-linux，amd64+arm64 rootless）→ **P2** WSL2 路徑（internal 在實機驗證:宣告 port 經 wslrelay 可達、未宣告 port 隔離）→ **P3** **執行期預設已從 `shared` 翻成 `bridge`**（spec 與 manifest 的 `NetworkMode::default()` 皆為 `Bridge`）。

> **inbound relay 的 fd 來源（重要實作細節）**:rootless 下 supervisor 在 init user ns、無 `CAP_SYS_ADMIN`，**不能** `setns(NEWNET)` 進 app netns（且執行緒不能 `setns(NEWUSER)`）。故由**已在 netns+userns 內為 root 的 holder 行程當 socket factory**:supervisor 經 SEQPACKET socketpair 送 `(proto,port)`，holder 在 netns 內建立連到 `127.0.0.1:guest` 的 socket，以 **SCM_RIGHTS** 傳回 fd，bidir 搬運留在 parent netns。holder 建 netns 時須**先單獨 `unshare(NEWUSER)`、在 unshare 前先取得真實 uid/gid 寫單行 map**（unshare 後 `getuid()` 會回 overflow id），再 `unshare(NEWNET)`。
>
> **bridge 出網（pasta）與 root 後端**:`pasta` 是 rootless-only——以 root 執行會自我停用、降權到 nobody 後又開不了 root 擁有的 ns。故 holder 與 pasta **一律以非特權 uid 執行**（rootless=呼叫者；WSL2 / VM / native-root 後端=**nobody**），netns 由該非特權 user ns 擁有，pasta 以 `--userns` 進入。root 後端 holder 降權時須在 `setuid` 後 `prctl(PR_SET_DUMPABLE,1)`，否則 `/proc/self/{setgroups,uid_map}` 變 root 擁有而寫不了。服務不受影響:root 後端服務仍以**真實 root** 只 `setns` 進 netns（不加入該 user ns）→ chown/gosu 照常。pasta 另須 `-t none -u none -T none -U none` 關閉預設的雙向 port 轉發（否則會在 netns 內搶占服務要綁的 port）。已於原生 Linux（CI）、實機 WSL2 與實機 WHP（micro-VM，見 §6 M7-c 的 appliance/guest-agent 前提修正）驗證 `internal` 隔離與 `bridge` 出網。
>
> **VM guest（appliance）跑 pasta 的三個前提**（缺一即 `bridge` 靜默降級 `internal`）：kernel `CONFIG_TUN`；`/dev/net/tun` 節點對非特權 uid 可開（appliance init 設 0666）；根檔案系統不能是 initramfs 的 rootfs（pasta 自我沙箱 `pivot_root` 在 rootfs 上一律 EINVAL——appliance init 開機早期 `switch_root` 到 tmpfs 根解掉）。另外 Windows host 打包的 bundle 記錄不了 unix 執行位，guest-agent 對 pasta exec `EACCES` 會複製到 tmp 補 0755 重試（`netns.rs` `start_pasta`）。

### Dockerfile build（`source: dockerfile`）— 設計

讓使用者直接給 Dockerfile，由 `chefer build` 代為建置成 image，省掉手動 `docker build` + `docker save`。

```yaml
services:
  app:
    image:
      source: dockerfile
      file: ./Dockerfile          # Dockerfile 路徑（normalize 絕對化）
      context: .                  # 選填：build context 目錄；省略 → Dockerfile 所在目錄（normalize 絕對化）
      platform: linux/amd64       # 選填：傳給 builder 的 --platform
      build_args:                 # 選填：KEY: VALUE → --build-arg
        VERSION: "1.2.3"
```

**做法（chefer-pack `dockerfile` 模組）**：chefer 本身不建 image——在**打包機**上偵測並呼叫既有的 container builder：
1. **偵測 builder**：依序試 `docker` → `podman` → `nerdctl` → `container`（首個可執行者；以 `--version` 探測）。前三者的 `build`/`save` CLI 相容（docker-archive）；macOS 上的 **OrbStack / Docker Desktop** 都提供 drop-in `docker` CLI，故直接走 `docker` 那條、無需特別處理。**Apple `container`**（開源 container 工具）：`build` 旗標相容（`-t`/`-f`/`--platform os/arch`/context），但 save 在 `image` 子命令下（`container image save -o`，產出 **OCI archive**——chefer 的 image 解析端 docker-archive 與 OCI archive 兩種都吃）。找不到任何一個 → 可行動錯誤，列出支援的 builder。
2. **build**：`<tool> build --platform <platform> [--build-arg K=V …] -f <dockerfile> -t <暫時 tag> <context>`，stdio 直通讓使用者看到建置過程；非零 → 透傳 builder 錯誤。
3. **save**：`<tool> save -o <tmp>/image.tar <暫時 tag>`（docker-archive）。
4. **接既有路徑**：把該 tar 當 `source: tar` 解析（`archive::extract_tar_to_dir` + `image::resolve_image`）→ 共用 repack。
5. best-effort `<tool> image rm <暫時 tag>` 清掉暫時 image。

**權衡（明示）**：`source: dockerfile` **不保證可重現**（Dockerfile 內 `apt`/`pip` 等每次拉到的版本可能不同；對齊 Docker 的本質）。要可重現請用 `source: image` 釘 digest。`--platform` 跨架構需 builder 端的 emulation（buildx/qemu binfmt）；建原生架構則免。執行期一如往常**不需** Docker——builder 只在打包時用到（與 `source: tar` 需要你先 `docker save` 的前提一致）。

**驗證**：`appcipe-spec` 接受 `source: dockerfile`（檢查 `file` 非空）；不適用 `check_image_reference`（那是 registry ref 專用）。

### 健康檢查（depends_on wait-until-ready）— 設計

服務可選 app 內欄位 **`healthcheck`**（appcipe-spec；寫進 manifest 的 `ServiceEntry.healthcheck`），對齊 Docker `HEALTHCHECK` 語意：

```yaml
services:
  db:
    image: redis:7.2-alpine
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]   # 或 ["CMD-SHELL", "..."]，或字串（= sh -c）
      interval: 2s        # 兩次檢查間隔（預設 2s）
      timeout: 5s         # 單次檢查逾時（預設 5s）
      retries: 10         # 連續失敗幾次才算 unhealthy（預設 10）
      start_period: 0s    # 寬限期：期間的失敗不計入 retries（預設 0s）
  app:
    image: ./app.tar
    depends_on: [db]      # db 有 healthcheck → 等 db healthy 才啟動 app
```

- **`test` 形式**（對齊 Docker）：字串 → `["/bin/sh","-c",s]`；`["CMD", argv…]` → 直接 argv；`["CMD-SHELL", s]` → `sh -c s`。正規化後存成 manifest 的 `CmdSpec`（`shell`/`argv`），duration 解析成毫秒（`interval_ms`/`timeout_ms`/`start_period_ms`）。
- **time 解析**：接受 `<n>(ms|s|m)` 或裸整數（= 秒）。
- **驗證**（appcipe-spec）：有 `healthcheck` 時 `test` 非空；`interval`/`timeout` > 0；`retries` ≥ 1；`start_period` ≥ 0。

**在容器內執行 test（nsenter + chroot）**：supervisor 已追蹤每個服務的 pid-ns init host pid（`init_pid`）。健康檢查 fork 一個子行程，**進入該服務的 namespaces 後 exec test**：
1. fork 前先開好 `/proc/<init>/ns/{user(僅 rootless),net,pid}` 與 `/proc/<init>/root` 的 fd。
2. 子行程：`setns(user)`（rootless 先做以取得該 userns 內 root 與 caps）→ `setns(net)`（與服務同網路，能連 `127.0.0.1:<port>`）→ `setns(pid)`（CLONE_NEWPID 只影響其後 fork）→ **再 fork** 使孫行程成為該 pid-ns 成員 → `fchdir(root_fd); chroot("."); chdir("/")`（容器 rootfs 成為 `/`，故 test 命令在映像內解析）→ exec test argv。
3. 父端對檢查行程套 `timeout` 上限（逾時 SIGKILL）；exit 0 = healthy，否則該次失敗。

進不去 namespace、開不了 fd 等屬基礎建設錯誤 → 直接 fail（非「檢查失敗」）。root 後端服務無獨立 user ns（`setns(user)` 略過）；rootless 與 root 路徑共用同一 runner。

### 主控台顯示（app 級 `console`）— 設計

stdio 模型不變：**全 app 最多一個互動終端**（唯一的 `terminal`/`both` 服務直通 stdin/stdout），其餘服務的 stdout/stderr 以 `[svc]` 前綴**彙整到同一個「共用主控台」**（即 chefer-runtime 自己的終端）。本節只決定「那個共用主控台要不要顯示」——主要影響 **Windows 雙擊啟動**時系統為 console-subsystem 執行檔新配的 console 視窗。

AppCipe 新增 app 級欄位 **`console`**（appcipe-spec enum `ConsoleMode`，serde 小寫；寫進 manifest `app.console`）：

- **`auto`（預設）**：依**聚合介面模式**決定——
  - 有 `terminal`/`both` 服務 → **顯示**（互動終端本來就需要那個視窗）。
  - 只有 `gui`（無終端）→ **隱藏**（乾淨的 GUI 體驗；關視窗即停 app）。
  - 全 `none`（無任何介面）→ **顯示**（共用主控台是唯一的停止介面）。
- **`shown`**：一律顯示。
- **`hidden`**：隱藏共用主控台。**驗證限制**：app 必須另有 `gui` 或 `terminal`/`both` 服務（否則啟動後無從停止）→ 全 `none` + `hidden` 在 `appcipe-spec` 驗證期報錯。

**runtime 行為**（`chefer-runtime`，啟動時、起服務前）：
- **全 `none` app**：一律保留共用主控台，並印一行提示（「此 app 沒有圖形或終端介面；在此視窗按 Ctrl+C 可停止」）——不論 `console` 設定，runtime 都不隱藏（防止舊/手刻 bundle 把唯一停止介面藏掉）。
- 其餘情況依上表決定是否隱藏。**隱藏只在 Windows 生效**（其他平台沒有「雙擊配 console」概念，終端由父行程繼承，不主動隱藏）。
- Windows 隱藏採 `ShowWindow(GetConsoleWindow(), SW_HIDE)`，但**僅當本行程是 console 的唯一擁有者**（`GetConsoleProcessList()==1`，即雙擊啟動才新配 console）；從既有 shell 啟動時 process list > 1，隱藏會連帶藏掉使用者自己的終端，故跳過。

### chefer-cli（bin）
- 子命令：
  - `init [dir]`：產生範本 appcipe.yml（不覆蓋既有檔）。
  - `check [path] [--format pretty|json|yaml]`：驗證 + 摘要（沿用現有表格 UI）。
  - `build [path] [--out dist] [--target <triple>]... [--kit-dir <dir>] [--dry-run] [--zstd-level N]`：load → pack → 對每個 target 找 runtime → assemble → 印出輸出路徑與大小。預設 target = host triple（編譯期 `BUILD_TARGET`）。
  - `run [path] [--build 之參數]`：build（單一 host target）後直接執行產物，stdio 直通。
  - `doctor [--kit-dir <dir>]...`：檢查目前 host 是否具備 build/run Chefer app 的基本條件；輸出每項 PASS/WARN/FAIL 與英文可行動建議。檢查 OS/arch、平台後端前提（Windows WSL2 + 規劃中的 WHP fallback host preflight、Linux root 或 unprivileged user namespaces）、kit 探索（沿用 `chefer_bundle::kit`，含 guest-agent/pasta/appliance/vz helper/whp helper）、Chefer 版本/build 資訊，以及平台預設 data 目錄是否可寫。任一 FAIL → exit code 非 0。
  - `inspect <single-file>`：讀 footer + 串流掃描 payload tar（不執行、不落地解壓），顯示 manifest 摘要、app network/console、payload 大小、**「Packed by chefer」**（取自 manifest 的 `app.builder_version`，舊 bundle 無此欄位則顯示 unknown）、每個 service 的 platform/layers/各層壓縮大小/persist/ports/mounts/interface/depends_on/healthcheck，以及內嵌 `agents/`、`vm/` 協力檔清單。
  - `version` / `upgrade`：repo = `TimLai666/chefer`（常數修正）。`upgrade` 經 HTTPS（rustls）自 GitHub Releases 取得**目前 host target 的完整 kit 壓縮包**並**同時替換 `chefer` 二進位與整個 `kit/`**（sha256 驗證、含 rollback），而不是只替換單一二進位——CLI 與 kit 永遠同版本、不會漂移。
    - asset 命名沿用 release workflow：`chefer_<tag>_<target>.zip`（Windows）或 `chefer_<tag>_<target>.tar.gz`（Linux/macOS）；`<tag>` 取自 GitHub Release tag/version，不在程式碼硬寫。
  - `selfrm [-y] [--clean-wsl]`：移除 chefer 本身——以 self-replace 的 `self_delete` 刪掉 CLI 執行檔、刪掉同層且確為 chefer kit 的 `kit/`、並盡力清掉安裝腳本加到 PATH 的設定（Unix 改 rc 檔、Windows 過濾使用者 PATH）。**不**動使用者打包出的 app 單檔或 app 持久化資料；新版 Windows packaged app 會在 runtime 結束時自行清理當次 WSL distro。舊版殘留的 `chefer-rt-*` 只有在 Windows 上明確傳 `--clean-wsl` 時才會透過 `vmm_backend::cleanup_distros()` 清理，且會在刪 kit/CLI 前先執行，失敗即中止。命名用 `selfrm` 而非 `uninstall`，以免誤讀成「chefer 去移除別的東西」。
- **安裝**：`scripts/install.sh`（Linux/macOS）與 `scripts/install.ps1`（Windows）一行安裝——偵測 OS/arch、抓對應 release asset、驗 sha256、解壓到 `~/.chefer`（Windows `%LOCALAPPDATA%\chefer`）、加進 PATH；完全不依賴既有 chefer，故可重裝救回壞掉的版本。`install.ps1` 全 ASCII（避開 Windows PowerShell 5.1 對無 BOM UTF-8 的 ANSI 誤判）。
    - 必須同時下載同名 `.sha256`，計算壓縮包 SHA-256 並比對後才解壓；`.sha256` 內容採 `sha256sum` 格式（`<hex>  <filename>`），只信任第一欄 64 位十六進位。
    - 解壓到同目錄暫存資料夾；安全檢查每個 archive entry（拒絕絕對路徑、Windows 前綴、`..`、空路徑）。解壓後必須剛好得到 `chefer_<tag>_<target>/` 根目錄，內含 `chefer[.exe]` 與 `kit/chefer-runtime-*`、`kit/guest-agent-x86_64`、`kit/guest-agent-aarch64`。
    - 驗證完整後，先以 `self_replace` 替換目前執行中的 `chefer`，再以暫存 `kit/` 原子性（同檔案系統 rename）替換目前執行檔旁的 `kit/`；若任一步失敗，錯誤訊息需指出可手動解壓 release kit 覆蓋安裝目錄。
    - 傳輸層受 TLS 保護，且 `.sha256` 可偵測下載損毀；但**不驗證發佈產物簽章**。供應鏈強化（防 release/帳號層級妥協）的後續方向：啟用 self_update 的 `signatures` feature + 內嵌 maintainer 簽章公鑰，對 Release 資產以 zipsign 簽署。
    - release workflow 在上傳前必須以 `scripts/verify-release-kit.sh` 驗證每個 kit 壓縮包與 `.sha256`：檔名安全、checksum 正確、唯一根目錄、host CLI、六個 runtime、兩個 guest-agent、兩個架構的 macOS appliance（kernel + initramfs）、兩個 VZ helper、兩個 WHP helper，且不含 symlink/special entry。workflow 先以 preflight 驗證 tag 存在於 `refs/tags/<tag>`、可 resolve 到 commit，且 tag 僅含 `A-Za-z0-9._-` 並以英數字開頭；`workflow_dispatch` dry-run 必須要求輸入既有 git tag，checkout 該 tag 後跑同一套六目標 build/package/verify，但只上傳 Actions artifacts、不掛到 GitHub Release；published release 則使用 release tag 並上傳 release assets。
- 錯誤輸出統一走 `anyhow` context；user-facing 摘要維持彩色表格。

## 7. 平台支援矩陣（v1 目標）

| 能力 | Linux | Windows | macOS |
|---|---|---|---|
| `chefer build`（產任意平台單檔，給定 kit）| ✅ | ✅ | ✅ |
| 單檔執行（linux/amd64,arm64 服務）| ✅ namespaces | ✅ WSL2；✅ whp（**實機跑真 bundle 到 `CHEFER_GUEST_EXIT=0`**：virtio-blk 傳 bundle/data，guest-agent 在 WHP VM 內組 rootfs 起服務；virtio-net + host→guest TCP 埠轉發實測可達） | 🔜 vz 骨架（明確錯誤）|
| GUI 服務 | ✅ X11/Wayland socket 直通 | ✅ WSLg | 🔜 |
| windows/amd64 容器 | ❌（驗證期報「尚未支援」）| ❌ 同左 | ❌ |

## 8. 測試策略

- 各 crate 單元測試；關鍵：spec 驗證矩陣、PortSpec/MountSpec 解析、footer roundtrip、pack 對合成 docker-archive 與 oci-archive 的解析（測試程式內建構最小映像 tar，不依賴 Docker）、whiteout 邏輯（純函式部分跨平台可測）。
- 整合測試（Windows host 可跑）：合成映像 → pack → assemble（用實際編出的 chefer-runtime.exe）→ 執行 `--dump-footer`、驗證解壓與 manifest。
- Linux 行為（namespaces、rootfs 組裝）在 CI ubuntu runner 上以整合測試驗證；本機可用 WSL2 輔助驗證。
- 原生 Linux E2E 使用 `scripts/linux-e2e.sh`（GitHub Actions: `Native Linux E2E`，matrix: `ubuntu-latest`（amd64）與 `ubuntu-24.04-arm`（arm64））：在非 root、非 WSL 的 Linux host 上以 Docker 建立真實映像並 `docker save` 成 tar，接著建置 host 原生 gnu target 的 runtime（免 musl C 交叉工具鏈——散佈用的 musl 靜態單檔由 release.yml 經 cross 另建、CI 另有 musl 靜態檢查）、`chefer build --target <host-gnu>` 成單檔並實際執行；驗證服務在 rootless user/pid namespaces 內（container euid=0、pid=1、uid_map 映射到 host uid）、persist_path 重啟後仍保留、`crash: fail_fast` exit code 透傳、以及 host≠guest TCP 埠映射可由 host 連線。arm64 目標以同一腳本在 GitHub hosted `ubuntu-24.04-arm` runner 上實跑。
- Linux appliance / QEMU E2E 使用 `scripts/build-appliance.sh` + `scripts/qemu-e2e.sh`（GitHub Actions: `appliance QEMU E2E`）：以指定的 Linux git tag/ref 產生 `chefer-vmlinuz-<arch>` 與 `chefer-initramfs-<arch>`，再用 QEMU + virtiofs 掛入真實 Chefer bundle/data，開機後由 initramfs 執行 bundle 內的 musl guest-agent；驗證 guest-agent 在 VM 內建立 namespaces（euid=0、pid=1、uid_map 證據）、persist_path 重啟保留、`fail_fast` exit code 透過 `CHEFER_GUEST_EXIT=<code>` 回傳，以及 host≠guest TCP forwarding 可由 host 連線。這條 E2E 不依賴 macOS，是啟用 VZ shim 前的可信前置。
- Linux GUI E2E 由同一腳本在 `CHEFER_E2E_GUI=1` 時啟用：host 端啟動 Xvfb，容器映像內執行真 X11 程式（`xmessage`），`interface_mode: gui` 需正確 bind `/tmp/.X11-unix` 並傳遞 `DISPLAY`，host 端以 `xwininfo` 確認視窗存在；另以 headless Weston + `wayland-info` 驗證 Wayland socket 與 `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` 傳遞。
- Windows WSL runtime lifecycle E2E 使用 `scripts/windows-wsl-cleanup-e2e.ps1`：在 Windows + WSL2 + Docker Desktop 上建置真實 Linux 映像，`docker save` 後打成 Windows 單檔；先以 `CHEFER_KEEP_WSL_DISTRO=1` 執行並確認預期的 `chefer-rt-*` distro 會保留，再 unregister 該 distro，接著正常執行並驗證 app exit 後 runtime 會移除同名 distro。script 讀 `wsl.exe -l -q` 時會移除 NUL，避免 PowerShell 直接 `.Trim()` 無法比對 distro 名稱。
- Windows WSLg GUI E2E 使用 `scripts/windows-wslg-e2e.ps1`：在 Windows + WSL2 + WSLg + Docker Desktop 的互動桌面上建置真實 X11 GUI 映像，`docker save` 後打成 Windows 單檔，執行時由 WSL2 後端建立 Chefer distro；script 以 Win32 top-level window enumeration 等待 `CheferWslgE2E` 視窗標題並要求程序正常結束，驗證 WSLg socket/env 與 Chefer GUI bind 實際可顯示。
