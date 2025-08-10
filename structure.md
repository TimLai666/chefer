# Chefer Project Structure

```
chefer/
├─ Cargo.toml                   # workspace
│
├─ crates/
│  ├─ appcipe-spec/             # 讀 appcipe：Serde 型別、解析、預設、驗證
│  │  ├─ src/
│  │  │   ├─ lib.rs
│  │  │   ├─ parse.rs
│  │  │   ├─ types.rs
│  │  │   └─ validate.rs
│  │  └─ Cargo.toml
│  │
│  ├─ appcipe-normalize/        # 舊欄位轉新欄位、套預設、路徑規則
│  │  ├─ src/
│  │  │   └─ main.rs
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-pack/              # 打包器：讀 appcipe → 解析 image tar
│  │  ├─ src/
│  │  │   ├─ api.rs
│  │  │   ├─ bundle.md
│  │  │   ├─ bundle.rs
│  │  │   ├─ image.rs
│  │  │   └─ lib.rs
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-assembler/         # 組裝器 → 產生單檔
│  │  ├─ src/
│  │  │   └─ main.rs
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-cli/               # 統一 CLI：`chefer init|build|run|format`
│  │  ├─ src/
│  │  │   └─ main.rs
│  │  ├─ build.rs
│  │  └─ Cargo.toml
│  │
│  ├─ chefer-runtime/           # 執行環境
│  │  ├─ src/
│  │  │   └─ main.rs
│  │  └─ Cargo.toml
│  │
│  ├─ guest-agent/              # VM 內 agent（PID1）：依 appcipe 啟服務、監控
│  │  ├─ src/
│  │  │   └─ main.rs
│  │  └─ Cargo.toml
│  │
│  └─ vmm-backend/              # KVM/HVF/WHPX 抽象 + 平台實作
│     ├─ src/
│     │   └─ main.rs
│     └─ Cargo.toml
│
└─ examples/
   ├─ appcipe.yml
   └─ appcipe_simple.yml
```