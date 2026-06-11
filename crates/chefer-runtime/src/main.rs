//! chefer-runtime — 單一執行檔的執行期主體。
//!
//! 流程（docs/DESIGN.md §6「chefer-runtime」）：
//! 讀自身 footer → 串流解壓 + sha256 驗證 → Manifest 載入 →
//! data dir 解析與 old_names 遷移 → 啟動埠代理 → vmm_backend::run_app →
//! 透傳 exit code → 清理暫存。

mod extract;
mod proxy;
mod run;

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(clap::Parser, Debug)]
#[command(name = "chefer-runtime", version, about = "Chefer 單檔執行期")]
struct Args {
    /// 指定解壓暫存目錄的父目錄（預設使用系統 temp）
    #[arg(long)]
    extract_dir: Option<PathBuf>,

    /// 保留解壓出的暫存目錄（預設退出即刪）
    #[arg(long)]
    keep_tmp: bool,

    /// 僅顯示 footer 資訊後退出（除錯用）
    #[arg(long)]
    dump_footer: bool,

    /// 顯示更詳細的執行紀錄（debug 等級）
    #[arg(long, short)]
    verbose: bool,
}

fn main() {
    let args = <Args as clap::Parser>::parse();

    let level = if args.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(level)
        .init();

    // 退出碼 = app 的 exit code；錯誤一律以 1 退出。
    // real_main 返回時 Extracted 已 drop（暫存已清），再呼叫 process::exit 才安全。
    match real_main(&args) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            tracing::error!("執行失敗：{err:#}");
            std::process::exit(1);
        }
    }
}

fn real_main(args: &Args) -> Result<i32> {
    let exe = std::env::current_exe().context("取得自身執行檔路徑失敗")?;

    let ft = chefer_bundle::Footer::read_from_file(&exe).with_context(|| {
        format!(
            "讀取單檔 footer 失敗：{}；此檔案可能不是由 `chefer build` 組裝的單一執行檔",
            exe.display()
        )
    })?;

    if args.dump_footer {
        println!(
            "footer: version={} flags={:#010b} (zstd={}) offset={} length={} sha256={}",
            ft.version,
            ft.flags,
            ft.is_zstd(),
            ft.offset,
            ft.length,
            hex::encode(ft.sha256)
        );
        return Ok(0);
    }

    let opts = extract::ExtractOptions {
        extract_parent: args.extract_dir.as_deref(),
        keep_tmp: args.keep_tmp,
    };
    let extracted = extract::extract_bundle(&exe, &ft, &opts)?;
    tracing::info!("bundle 已解壓至 {}", extracted.bundle_dir.display());

    let code = run::run(&extracted.bundle_dir, args.keep_tmp)?;
    // extracted 在此 scope 結束時 drop：未指定 --keep-tmp 時自動刪除暫存目錄。
    Ok(code)
}
