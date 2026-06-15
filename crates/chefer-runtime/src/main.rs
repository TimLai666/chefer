//! chefer-runtime — 單一執行檔的執行期主體。
//!
//! 流程（docs/DESIGN.md §6「chefer-runtime」）：
//! 讀自身 footer → 串流解壓 + sha256 驗證 → Manifest 載入 →
//! data dir 解析與 old_names 遷移 → 啟動埠代理 → vmm_backend::run_app →
//! 透傳 exit code → 清理暫存。

mod console;
mod extract;
mod proxy;
mod run;

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(clap::Parser, Debug)]
#[command(name = "chefer-runtime", version, about = "Chefer single-file runtime")]
struct Args {
    /// Extraction parent directory: the bundle cache root by default, or the temp parent with --no-cache/--keep-tmp
    #[arg(long)]
    extract_dir: Option<PathBuf>,

    /// Keep the extracted temp directory (deleted on exit by default); implies --no-cache
    #[arg(long)]
    keep_tmp: bool,

    /// Disable the persistent bundle cache; extract to a temp directory each run (deleted on exit)
    #[arg(long)]
    no_cache: bool,

    /// Print footer info and exit (for debugging)
    #[arg(long)]
    dump_footer: bool,

    /// Show more detailed logs (debug level)
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
            tracing::error!("execution failed: {err:#}");
            std::process::exit(1);
        }
    }
}

fn real_main(args: &Args) -> Result<i32> {
    let exe =
        std::env::current_exe().context("failed to get the path of the running executable")?;

    let ft = chefer_bundle::Footer::read_from_file(&exe).with_context(|| {
        format!(
            "failed to read the single-file footer: {}; this file may not be a single-file executable assembled by `chefer build`",
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

    // 預設走持久化 bundle 快取（依 payload sha256 跳過重複解壓）；
    // --no-cache / --keep-tmp 則每次解到暫存目錄。
    let extracted = if args.no_cache || args.keep_tmp {
        let opts = extract::ExtractOptions {
            extract_parent: args.extract_dir.as_deref(),
            keep_tmp: args.keep_tmp,
        };
        extract::extract_bundle(&exe, &ft, &opts)?
    } else {
        let cache_root = args
            .extract_dir
            .clone()
            .unwrap_or_else(extract::default_cache_root);
        extract::extract_bundle_cached(&exe, &ft, &cache_root)?
    };
    tracing::info!("bundle ready at {}", extracted.bundle_dir.display());

    let code = run::run(&extracted.bundle_dir, args.keep_tmp)?;
    // extracted 在此 scope 結束時 drop：暫存路徑自動刪除；快取路徑保留供下次重用。
    Ok(code)
}
