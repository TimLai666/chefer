//! chefer-assembler CLI（除錯用；正式入口是 chefer-cli 的 `chefer build`）。
//!
//! 用法：
//! `chefer-assembler --runtime <path> --bundle <dir> --out <path> [--zstd-level N]`
//!
//! runtime 執行檔請自行指定路徑（kit 探索由 chefer-cli 透過
//! `chefer_bundle::kit` 處理，本工具不重複實作）。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use chefer_assembler::{AssembleOptions, assemble};

/// 將 chefer-runtime 執行檔與 bundle 目錄組成單一執行檔。
#[derive(Parser, Debug)]
#[command(name = "chefer-assembler", version)]
struct Cli {
    /// chefer-runtime 執行檔路徑（例：target/release/chefer-runtime.exe，
    /// 或 kit 目錄中的 chefer-runtime-<target-triple>[.exe]）
    #[arg(long)]
    runtime: PathBuf,

    /// bundle 目錄（必須含 manifest.json，由 chefer-pack 產生）
    #[arg(long)]
    bundle: PathBuf,

    /// 輸出單一執行檔路徑（會覆蓋既有檔案）
    #[arg(long)]
    out: PathBuf,

    /// zstd 壓縮等級（1..=22；預設 3，即 zstd 函式庫預設；0 亦表示函式庫預設）
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let opts = AssembleOptions {
        zstd_level: cli.zstd_level,
    };
    match assemble(&cli.runtime, &cli.bundle, &cli.out, &opts) {
        Ok(report) => {
            println!("已輸出單一執行檔：{}", cli.out.display());
            println!("  payload offset : {} bytes", report.payload_offset);
            println!("  payload length : {} bytes", report.payload_len);
            println!("  payload sha256 : {}", report.sha256_hex());
            println!("  輸出檔總大小   : {} bytes", report.out_size);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("錯誤：{e:#}");
            ExitCode::FAILURE
        }
    }
}
