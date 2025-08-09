# Chefer Project Structure

```
chefer/
├─ Cargo.toml                   # workspace
│
├─ crates/
│  ├─ appcipe-spec/             # ✅ 放「讀 appcipe」的程式（Serde 型別+解析+預設+驗證）
│  │  ├─ src/lib.rs
│  │  └─ schemas/               # (選) JSON Schema / 範例
│  │
│  ├─ appcipe-normalize/        # (選) 把舊欄位轉新欄位、套預設、路徑規則
│  │  └─ src/lib.rs             #   e.g. terminal/gui -> interface_mode；預設值；校驗錯誤訊息
│  │
│  ├─ chefer-pack/              # 打包器：讀 appcipe → 解析 image tar → 產生單檔
│  │  └─ src/lib.rs
│  │
│  ├─ chefer-launch/            # 執行器：載入內嵌的 appcipe（bytes）→ 起 microVM/GUI
│  │  └─ src/lib.rs
│  │
│  ├─ vmm-backend/              # KVM/HVF/WHPX 抽象 + 平台實作
│  │  └─ src/lib.rs
│  │
│  ├─ guest-agent/              # VM 內的 agent（PID1）：依 appcipe 啟服務、監控崩潰
│  │  └─ src/main.rs
│  │
│  └─ chefer-cli/               # 統一 CLI：`chefer init|build|run|format`
│     └─ src/main.rs
│
└─ examples/
   └─ appcipe.yml
```