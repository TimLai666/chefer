# Chefer Project Structure

```
chefer/
├─ Cargo.toml                   # workspace
├─ .cargo/
│  └─ config.toml               # musl target 統一以 rust-lld 連結（跨 host 免 musl cc）
│
├─ crates/
│  ├─ appcipe-spec/             # 讀 appcipe：Serde 型別、解析、驗證（不做路徑正規化）
│  │  ├─ src/
│  │  │   ├─ lib.rs
│  │  │   ├─ parse.rs           # from_file/from_str（解析 + 驗證）
│  │  │   ├─ types.rs           # AppCipe / Service 等 serde 型別
│  │  │   └─ validate.rs        # 驗證規則（收集所有錯誤一次回報）
│  │  └─ Cargo.toml
│  │
│  ├─ appcipe-normalize/        # 正規化：host 路徑絕對化、舊欄位遷移；正式載入入口 load()
│  │  ├─ src/
│  │  │   └─ lib.rs
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-bundle/            # 跨 crate 共用協定（單一定義處）：manifest、footer、kit、佈局
│  │  ├─ src/
│  │  │   ├─ lib.rs
│  │  │   ├─ manifest.rs        # manifest.json schema 的 serde 型別
│  │  │   ├─ footer.rs          # 單檔尾端 80-byte footer 統一讀寫
│  │  │   ├─ kit.rs             # runtime kit（預編譯二進位）搜尋
│  │  │   ├─ layout.rs          # bundle 目錄佈局的路徑輔助
│  │  │   ├─ ports.rs           # "host:guest[/proto]" 埠映射解析
│  │  │   ├─ mounts.rs          # "<host>:<guest>" 掛載解析（相容 C:\ 磁碟代號）
│  │  │   └─ topo.rs            # depends_on 拓撲排序（決定啟動順序）
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-pack/              # 打包器：image tar → 以「層」儲存的 bundle + manifest.json
│  │  ├─ src/
│  │  │   ├─ lib.rs             # pack() 入口
│  │  │   ├─ archive.rs         # image tar 安全解壓（路徑安全檢查）
│  │  │   ├─ image.rs           # docker-archive / OCI layout 偵測解析、multi-arch 挑選
│  │  │   ├─ layers.rs          # 層 blob 串流重壓（gzip/zstd/未壓縮 → zstd）+ diff_id 驗證
│  │  │   └─ convert.rs         # appcipe 型別 → manifest 型別轉換
│  │  ├─ tests/
│  │  │   └─ pack_tests.rs      # 以測試內合成的最小映像驗證 pack（不依賴 Docker）
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-assembler/         # 組裝器：runtime + zstd(tar(bundle)) + footer → 單一執行檔
│  │  ├─ src/
│  │  │   ├─ lib.rs             # assemble() 入口（全程串流、邊寫邊算 sha256）
│  │  │   └─ main.rs            # 除錯用 bin（正式入口是 chefer-cli）
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-cli/               # 統一 CLI：`chefer init|check|build|run|inspect|version|upgrade`
│  │  ├─ src/
│  │  │   ├─ main.rs            # 參數定義與分派
│  │  │   ├─ ui.rs              # 彩色表格 UI 輔助
│  │  │   └─ commands/          # 一檔一命令（init/check/build/run/inspect/version/upgrade）
│  │  ├─ tests/
│  │  │   └─ cli_e2e.rs         # 端到端整合測試（測試內合成映像與假 kit，不依賴 Docker）
│  │  ├─ build.rs               # 注入 BUILD_TIME / BUILD_TARGET
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-runtime/           # 單檔執行期主體：footer → 解壓驗證 → 埠代理 → 後端
│  │  ├─ src/
│  │  │   ├─ main.rs
│  │  │   ├─ extract.rs         # 串流解壓 + sha256 驗證（不整包讀進記憶體）
│  │  │   ├─ proxy.rs           # host≠guest 埠代理（TCP 雙向轉送、UDP relay）
│  │  │   └─ run.rs             # data dir 解析、old_names 遷移、呼叫 vmm_backend
│  │  └─ Cargo.toml
│  │
│  ├─ guest-agent/              # Linux 環境內 agent：由層組 rootfs、啟動與監控服務（musl 靜態）
│  │  ├─ src/
│  │  │   ├─ lib.rs             # RunConfig + run_bundle()（Linux 後端 in-process 呼叫）
│  │  │   ├─ main.rs            # bin 入口（WSL2 / VM 內執行）
│  │  │   ├─ rootfs.rs          # 層解壓、whiteout 套用、rootfs 快取
│  │  │   ├─ whiteout.rs        # OCI whiteout 檔名解析（純函式）
│  │  │   ├─ exec.rs            # user/mount/pid namespaces + pivot_root + exec
│  │  │   ├─ supervisor.rs      # fail_fast 服務監控
│  │  │   └─ applets.rs         # busybox 風格 mount/umount applet（WSL distro 必需）
│  │  └─ Cargo.toml
│  │
│  └─ vmm-backend/              # 平台執行後端抽象（ExecBackend trait）與實作
│     ├─ src/
│     │   ├─ lib.rs             # backends() / run_app()：依平台挑第一個可用後端
│     │   ├─ namespaces.rs      # Linux：rootless namespaces（in-process 呼叫 guest-agent）
│     │   ├─ wsl2.rs            # Windows：chefer 專用最小 WSL distro 內跑 guest-agent
│     │   ├─ wsl_util.rs        # 路徑轉換、distro 命名、最小 rootfs tar（純函式可測）
│     │   └─ vz.rs              # macOS：Virtualization.framework 骨架（v1 回報未支援）
│     └─ Cargo.toml
│
├─ docs/
│  ├─ DESIGN.md                 # 架構契約（所有 crate 的單一事實來源）
│  └─ QUICKSTART.zh-TW.md       # 繁中快速上手
│
├─ examples/
│  ├─ appcipe.yml               # 完整註解版範例
│  └─ appcipe_simple.yml        # 最小可用範例
│
└─ .github/workflows/
   ├─ ci.yml                    # 三平台 build + test、musl guest-agent 靜態連結檢查
   └─ release.yml               # 發佈時建置 6 個 target 並上傳各平台完整 kit
```
