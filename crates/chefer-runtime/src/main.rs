// src/main.rs
mod extract;
mod footer;
mod run;
mod util;

use anyhow::Result;
use std::path::PathBuf;
use tracing_subscriber::FmtSubscriber;

#[derive(clap::Parser, Debug)]
#[command(name = "chefer-runtime", version, about = "Chefer Runtime Stub")]
struct Args {
    /// 指定暫存解壓目錄（預設使用系統 temp）
    #[arg(long)]
    extract_dir: Option<PathBuf>,

    /// 保留 temp 目錄（預設退出即刪）
    #[arg(long)]
    keep_tmp: bool,

    /// 僅顯示 footer 資訊後退出（除錯用）
    #[arg(long)]
    dump_footer: bool,
}

fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder().with_target(false).finish();
    tracing::subscriber::set_global_default(subscriber).ok();

    let args = <Args as clap::Parser>::parse();
    let exe = std::env::current_exe()?;

    let ft = footer::Footer::read_from_exe(&exe)?;
    if args.dump_footer {
        println!(
            "footer: version={} flags={:#010b} offset={} length={} sha256={}",
            ft.version,
            ft.flags,
            ft.offset,
            ft.length,
            hex::encode(ft.sha256)
        );
        return Ok(());
    }

    let keep_dir = if args.keep_tmp {
        args.extract_dir.as_deref()
    } else {
        args.extract_dir.as_deref()
    };
    let extracted = extract::extract_bundle(&exe, &ft, keep_dir)?;
    tracing::info!("bundle extracted at {}", extracted.bundle_dir.display());

    let ctx = run::RuntimeContext {
        bundle_dir: camino::Utf8PathBuf::from_path_buf(extracted.bundle_dir.clone()).unwrap(),
    };
    run::run(&ctx)?;

    // TempDir 會在 drop 時清除；若 keep_tmp=true，你可以改成不釋放讓它保留
    Ok(())
}
