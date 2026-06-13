# Chefer 設計契約（v1）

本文件是所有 crate 共同遵守的「單一事實來源」。任何跨 crate 的格式、API、行為變更都必須先改這份文件。

## 0. 全域原則

- **依賴版本**：新增依賴一律用 `cargo add`（由 crates.io 解析），**禁止手寫猜測版本號**。專案自身版本號不主動調整。
- **可編譯性**：整個 workspace 必須在 Windows / Linux / macOS 三平台都能 `cargo build`。平台限定邏輯用 `#[cfg(target_os = "...")]` 隔離，其他平台給出可編譯的 stub（回傳明確錯誤）。
- **語言**：程式註解與使用者訊息以繁體中文為主（與既有程式碼一致）；錯誤訊息需可行動（說明缺什麼、怎麼補）。
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
├─ agents/                        # （選）內嵌 guest-agent 靜態 musl 二進位
│  ├─ guest-agent-x86_64          # 對應 linux/amd64
│  └─ guest-agent-aarch64         # 對應 linux/arm64
└─ services/
   └─ <svc>/
      └─ layers/
         ├─ 0000-<diffid前12碼>.tar.zst   # 依序的 diff 層；內容 = zstd(未壓縮 layer tar)
         ├─ 0001-<diffid前12碼>.tar.zst
         └─ ...
```

- 層檔名：`{序號:04}-{diff_id去掉"sha256:"後取前12碼}.tar.zst`。
- `agents/` 規則：建置 Windows / macOS 目標的單檔時**必須**內嵌對應 guest 架構的 agent（缺少時 build 報錯並說明如何取得 kit）；Linux 目標可省略（Linux 後端 in-process 執行）。

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
    "generated_at_utc": "2026-06-11T00:00:00Z"
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
      "depends_on": []
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
- layout 輔助：`manifest_path(bundle_dir)`、`service_layers_dir(bundle_dir, svc)`、`agents_dir(bundle_dir)`、`guest_agent_name(arch)`、`platform_to_arch(platform)`、`layer_file_name(idx, diff_id)`。
- `topo_sort(services) -> Result<Vec<&ServiceEntry>>`（depends_on 拓撲排序；偵測循環）。
- `kit` 模組（**kit 探索統一在此**，pack/assembler/cli 共用）：`default_kit_dirs()`、`find_runtime(kit_dirs, target, allow_plain)`、`find_guest_agent(kit_dirs, arch)`、`runtime_file_name(target)`、`not_found_help(...)`。

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
  - env key：`[A-Za-z_][A-Za-z0-9_]*`。
  - `image.source` 目前僅支援 `tar`（dockerfile/image 回報「尚未支援」）。
- `AppCipe` 衍生 `Clone`。

### appcipe-normalize（改為 lib）
- `pub fn normalize(app: &mut AppCipe, base: &Path) -> anyhow::Result<()>`：
  - host 路徑絕對化（image.file、mounts 左半、data_dir）——沿用既有 rsplitn 邏輯。
  - 舊欄位遷移（目前：YAML 層級 `crash_policy` → `crash`；以 serde alias 或前置轉換實作，列入測試）。
- `pub fn load(path: &Path) -> anyhow::Result<AppCipe>`：讀檔 → 解析 → normalize → validate。CLI 與 pack 一律走這裡。

### chefer-pack（lib）
- `pub struct PackOptions { pub out_dir: PathBuf, pub clean: bool, pub write_original_yml: bool, pub kit_dirs: Vec<PathBuf>, pub require_agents: bool, pub zstd_level: i32 }`
- `pub struct PackResult { pub bundle_dir: PathBuf, pub manifest: chefer_bundle::Manifest }`
- `pub fn pack(app: &AppCipe, opts: &PackOptions) -> anyhow::Result<PackResult>`
- 行為：
  0. image tar 先安全解壓到暫存目錄再解析（OCI blobs 需隨機存取）；各層 blob 的解壓/雜湊/重壓全程串流。
  1. 對每個 service 讀 image tar，**自動偵測格式**（看內容不看副檔名）：
     - docker-archive：根目錄有 `manifest.json`（JSON 陣列，元素含 `Config`/`Layers`）。
     - oci-archive：根目錄有 `index.json` + `blobs/`（layout 可含 `oci-layout`）。
  2. multi-arch：oci index 依 service `platform` 挑 manifest；找不到該平台→報錯列出可用平台。docker-archive 多筆 entry 時亦同（依 config 的 os/architecture）。
  3. 層資料可能是 gzip(`+gzip`)、zstd(`+zstd`) 或未壓縮——一律**解到未壓縮 tar 再以 zstd 重壓**寫入 bundle；同時驗證/計算 diff_id（未壓縮 tar 的 sha256）與 config 的 `rootfs.diff_ids` 一致。
  4. 從 image config 抽出 `ImageConfig`（Entrypoint/Cmd/Env/WorkingDir/User/ExposedPorts）。
  5. 寫 manifest.json（用 chefer-bundle 型別）。
  6. 從 kit 複製 guest-agent 到 `agents/`（依 services 用到的 arch 去重；`require_agents=false` 時缺少僅警告）。
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
- 埠代理：對每個 `host != guest` 的 PortSpec 啟一條 thread：TCP `listen 127.0.0.1:host → connect 127.0.0.1:guest` 雙向轉送；UDP 簡單 relay（記住最後來源）。`host == guest` 不代理。
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
    fn availability(&self) -> Availability;
    fn run(&self, ctx: &AppRunContext) -> anyhow::Result<i32>; // app 整體 exit code
}
pub fn backends() -> Vec<Box<dyn ExecBackend>>;        // 依平台排序
pub fn run_app(ctx: &AppRunContext) -> anyhow::Result<i32>; // 取第一個 Available 的後端執行；全部不可用→彙整原因報錯
```
- **Linux**（`namespaces`）：可用性 = `/proc/self/ns/user` 存在且允許 unprivileged userns（讀 `/proc/sys/kernel/unprivileged_userns_clone` 若存在）。執行 = in-process 呼叫 `guest_agent::run_bundle`。
- **Windows**（`wsl2`）：可用性 = `wsl.exe --status` 成功且支援 WSL2。執行：
  1. agent 二進位 = `bundle/agents/guest-agent-<arch>`（缺→明確錯誤）。
  2. distro 名 = `chefer-rt-<agent sha256 前 8 碼>`；不存在時：產生最小 rootfs tar（標準 FHS 目錄 + `/bin/guest-agent` + `/etc/wsl.conf`（automount enabled + `options="metadata"`）+ `/etc/passwd` + **`/bin/mount`、`/bin/umount` → guest-agent 的 symlink** + **`/tmp/.X11-unix` → `/mnt/wslg/.X11-unix` symlink**，讓 WSLg 存在時 X11 client 可找到 socket），`wsl --import <distro> %LOCALAPPDATA%\chefer\wsl\<distro> <tar> --version 2`。
     **重要（實測根因）**：WSL init 啟動 distro 時會執行 distro 內的 `/bin/mount` 來掛載 drvfs（呼叫形如 `mount -i -t 9p C:\ /mnt/c -o msize=65536,trans=fd,rfdno=5,wfdno=5,cache=mmap,noatime,aname=drvfs;...`，9p 連線經繼承的 fd）。缺 mount 時 automount 與 interop 全滅。guest-agent 以 busybox 風格 applet（argv[0] 派發，`applets.rs`）提供 mount/umount。
  3. 以 `wsl -d <distro> --user root --exec /bin/guest-agent run --bundle <wsl路徑> --data <wsl路徑> --cache /var/lib/chefer/cache` 執行；rootfs 快取一律放 distro 內 ext4（/mnt/c 的 drvfs 上 symlink/hardlink/權限不可靠且 I/O 慢）；Windows 路徑以純函式轉換（`C:\foo` → `/mnt/c/foo`；UNC/相對路徑報錯），不依賴 wslpath；stdio 直通；exit code 透傳。
  4. 注意命令注入：所有外部參數都走 argv 陣列（`std::process::Command` 個別 arg），絕不組 shell 字串。
  5. **網路（實測）**：WSL2 的 localhost 轉送（wslrelay）只綁 IPv6 `[::1]`；runtime 埠代理後端因此「先試 127.0.0.1、再退 [::1]」，且對 `host == guest` 的 TCP 埠在 Windows 上加 best-effort 的 IPv4 補橋（127.0.0.1:port → [::1]:port）。WSL 的 localhost 轉送不支援 UDP——Windows 上跨 WSL 的 UDP 埠映射目前無法生效（文件需註明限制）。
- **macOS**（`vz`）：v1 骨架——`availability()` 檢查 OS 版本後仍回 `Unavailable("macOS 後端需要 guest kit（kernel+initrd），將於後續版本提供；目前請於 Linux 或 Windows 執行")`。程式碼結構保留 trait 實作位置。

### guest-agent（lib + bin；Linux 專屬邏輯 cfg 隔離）
- lib：`pub struct RunConfig { pub bundle_dir: PathBuf, pub data_dir: PathBuf, pub cache_dir: Option<PathBuf>, pub keep_rootfs: bool }`
  `pub fn run_bundle(cfg: &RunConfig) -> anyhow::Result<i32>`（非 Linux 回傳明確錯誤）。
- bin：`guest-agent run --bundle <dir> --data <dir> [--cache <dir>] [--keep-rootfs]`；另提供 `guest-agent assemble-rootfs`（除錯）。
- rootfs 組裝：
  - 目的地：`cache_dir`（預設 `<data_dir>/.rootfs-cache`）`/<svc>-<chain_hash12>/`，chain_hash = sha256(diff_id 以 `\n` 串接)。已存在且含 `.complete` 標記 → 直接重用。
  - 依序解每層 zstd tar：**whiteout 處理**——`.wh.<name>` → 刪除對應項；`.wh..wh..opq` → 清空該目錄既有內容；其餘正常解（保留 symlink/hardlink/權限；路徑安全檢查同 §0）。
  - 解完寫 `.complete`。
- 服務啟動（Linux）：
  - 依 `topo_sort` 順序啟動（v1：先後順序即依賴語意，無健康檢查；文件註明）。
  - 每個服務：`unshare(user+mount+pid)`（uid/gid map 成 root）→ 掛 `/proc` → bind `/dev/{null,zero,random,urandom,tty}`、`/dev/pts`、`/dev/shm`(tmpfs) → bind persist（`<data_dir>/data/<svc>` ↔ persist_path，host 端先 `create_dir_all`）→ bind mounts（host 路徑不存在→啟動前報錯）→ interface_mode 含 gui 時 bind `/tmp/.X11-unix` 與 `$XDG_RUNTIME_DIR/wayland-*` socket（存在且為 socket 才掛；WSLg 內若未提供 `XDG_RUNTIME_DIR` 但 `/mnt/wslg/runtime-dir` 存在，則以該目錄作為 Wayland fallback）並傳遞 `DISPLAY`/`XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`（`WAYLAND_DISPLAY` 僅沿用實際已掛 socket，否則取排序後第一個）→ `pivot_root` → `chdir(workdir)` → env 合併（§3）→ exec 有效命令。
  - 網路：**不** unshare netns（共享網路 → ports 直接生效，WSL2 下由 localhost forwarding 對外）。
  - 監控：任一子行程結束且 exit ≠ 0 → 終止其餘全部（SIGTERM → 等 5s → SIGKILL）→ 回傳該 exit code（fail_fast）。全部正常結束 → 0。
  - `interface_mode=terminal/both`：該服務 stdio 直通（v1：所有服務 stdout/stderr 都加 `[svc]` 前綴轉發；terminal 服務 stdin 直通——僅允許一個服務宣告 terminal/both，多個→驗證期報錯）。
- musl 靜態建置：`cargo build -p guest-agent --target x86_64-unknown-linux-musl --release` 必須可行（避免依賴需要 cc 的 crate；zstd 解壓用純 Rust 的 ruzstd）。musl 目標的 linker 統一為 rust-lld（`.cargo/config.toml` 已設定，跨 host 一致）。

### chefer-cli（bin）
- 子命令：
  - `init [dir]`：產生範本 appcipe.yml（不覆蓋既有檔）。
  - `check [path] [--format pretty|json|yaml]`：驗證 + 摘要（沿用現有表格 UI）。
  - `build [path] [--out dist] [--target <triple>]... [--kit-dir <dir>] [--dry-run] [--zstd-level N]`：load → pack → 對每個 target 找 runtime → assemble → 印出輸出路徑與大小。預設 target = host triple（編譯期 `BUILD_TARGET`）。
  - `run [path] [--build 之參數]`：build（單一 host target）後直接執行產物，stdio 直通。
  - `inspect <single-file>`：讀 footer + 解出 manifest.json 摘要（不執行）。
  - `version` / `upgrade`：repo = `TimLai666/chefer`（常數修正）。`upgrade` 經 HTTPS（rustls）自 GitHub Releases 取得**目前 host target 的完整 kit 壓縮包**並原地替換，而不是只替換 `chefer` 單一二進位。
    - asset 命名沿用 release workflow：`chefer_<tag>_<target>.zip`（Windows）或 `chefer_<tag>_<target>.tar.gz`（Linux/macOS）；`<tag>` 取自 GitHub Release tag/version，不在程式碼硬寫。
    - 必須同時下載同名 `.sha256`，計算壓縮包 SHA-256 並比對後才解壓；`.sha256` 內容採 `sha256sum` 格式（`<hex>  <filename>`），只信任第一欄 64 位十六進位。
    - 解壓到同目錄暫存資料夾；安全檢查每個 archive entry（拒絕絕對路徑、Windows 前綴、`..`、空路徑）。解壓後必須剛好得到 `chefer_<tag>_<target>/` 根目錄，內含 `chefer[.exe]` 與 `kit/chefer-runtime-*`、`kit/guest-agent-x86_64`、`kit/guest-agent-aarch64`。
    - 驗證完整後，先以 `self_replace` 替換目前執行中的 `chefer`，再以暫存 `kit/` 原子性（同檔案系統 rename）替換目前執行檔旁的 `kit/`；若任一步失敗，錯誤訊息需指出可手動解壓 release kit 覆蓋安裝目錄。
    - 傳輸層受 TLS 保護，且 `.sha256` 可偵測下載損毀；但**不驗證發佈產物簽章**。供應鏈強化（防 release/帳號層級妥協）的後續方向：啟用 self_update 的 `signatures` feature + 內嵌 maintainer 簽章公鑰，對 Release 資產以 zipsign 簽署。
    - release workflow 在上傳前必須以 `scripts/verify-release-kit.sh` 驗證每個 kit 壓縮包與 `.sha256`：檔名安全、checksum 正確、唯一根目錄、host CLI、六個 runtime、兩個 guest-agent，且不含 symlink/special entry。workflow 先以 preflight 驗證 tag 存在於 `refs/tags/<tag>`、可 resolve 到 commit，且 tag 僅含 `A-Za-z0-9._-` 並以英數字開頭；`workflow_dispatch` dry-run 必須要求輸入既有 git tag，checkout 該 tag 後跑同一套六目標 build/package/verify，但只上傳 Actions artifacts、不掛到 GitHub Release；published release 則使用 release tag 並上傳 release assets。
- 錯誤輸出統一走 `anyhow` context；user-facing 摘要維持彩色表格。

## 7. 平台支援矩陣（v1 目標）

| 能力 | Linux | Windows | macOS |
|---|---|---|---|
| `chefer build`（產任意平台單檔，給定 kit）| ✅ | ✅ | ✅ |
| 單檔執行（linux/amd64,arm64 服務）| ✅ namespaces | ✅ WSL2 | 🔜 vz 骨架（明確錯誤）|
| GUI 服務 | ✅ X11/Wayland socket 直通 | ✅ WSLg | 🔜 |
| windows/amd64 容器 | ❌（驗證期報「尚未支援」）| ❌ 同左 | ❌ |

## 8. 測試策略

- 各 crate 單元測試；關鍵：spec 驗證矩陣、PortSpec/MountSpec 解析、footer roundtrip、pack 對合成 docker-archive 與 oci-archive 的解析（測試程式內建構最小映像 tar，不依賴 Docker）、whiteout 邏輯（純函式部分跨平台可測）。
- 整合測試（Windows host 可跑）：合成映像 → pack → assemble（用實際編出的 chefer-runtime.exe）→ 執行 `--dump-footer`、驗證解壓與 manifest。
- Linux 行為（namespaces、rootfs 組裝）在 CI ubuntu runner 上以整合測試驗證；本機可用 WSL2 輔助驗證。
- 原生 Linux E2E 使用 `scripts/linux-e2e.sh`（GitHub Actions: `Native Linux E2E`，matrix: `ubuntu-latest`/`x86_64-unknown-linux-musl` 與 `ubuntu-24.04-arm`/`aarch64-unknown-linux-musl`）：在非 root、非 WSL 的 Linux host 上以 Docker 建立真實映像並 `docker save` 成 tar，接著建置 Linux musl runtime、`chefer build --target <linux-musl>` 成 release-like 單檔並實際執行；驗證服務在 rootless user/pid namespaces 內（container euid=0、pid=1、uid_map 映射到 host uid）、persist_path 重啟後仍保留、`crash: fail_fast` exit code 透傳、以及 host≠guest TCP 埠映射可由 host 連線。arm64 目標以同一腳本在 GitHub hosted `ubuntu-24.04-arm` runner 上實跑。
- Linux GUI E2E 由同一腳本在 `CHEFER_E2E_GUI=1` 時啟用：host 端啟動 Xvfb，容器映像內執行真 X11 程式（`xmessage`），`interface_mode: gui` 需正確 bind `/tmp/.X11-unix` 並傳遞 `DISPLAY`，host 端以 `xwininfo` 確認視窗存在；另以 headless Weston + `wayland-info` 驗證 Wayland socket 與 `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` 傳遞。
- Windows WSLg GUI E2E 使用 `scripts/windows-wslg-e2e.ps1`：在 Windows + WSL2 + WSLg + Docker Desktop 的互動桌面上建置真實 X11 GUI 映像，`docker save` 後打成 Windows 單檔，執行時由 WSL2 後端建立/重用 Chefer distro；script 以 Win32 top-level window enumeration 等待 `CheferWslgE2E` 視窗標題並要求程序正常結束，驗證 WSLg socket/env 與 Chefer GUI bind 實際可顯示。
