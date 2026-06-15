//! `chefer run` — 以本機 target 建置單一執行檔後直接執行。
//!
//! 參數同 build，但只允許 host target；stdio 直通（inherit），
//! 並把產物的 exit code 透傳給呼叫端（由 main 以 process::exit 結束）。

use anyhow::{Context, Result, anyhow};
use owo_colors::OwoColorize;

use crate::BuildOpts;
use crate::commands::build;

/// 建置（host target）後執行產物；回傳產物的 exit code。
pub fn cmd_run(opts: &BuildOpts) -> Result<i32> {
    // 只建置本機 target（targets 留空 → cmd_build 預設 host）。
    // local_run=true：本機建置後立即在本機執行，故 mount host 路徑檢查維持嚴格（非 VM 後端時）。
    let artifacts = build::cmd_build(opts, &[], false, true)?;
    let artifact = artifacts
        .first()
        .ok_or_else(|| anyhow!("internal error: build produced no artifacts"))?;

    println!(
        "{}  {}",
        "🚀 Run".green().bold(),
        artifact.path.display().to_string().blue()
    );

    let status = std::process::Command::new(&artifact.path)
        .status()
        .with_context(|| format!("failed to run artifact: {}", artifact.path.display()))?;

    // exit code 透傳；被信號終止（Unix 上 code() 為 None）時以 130 表示中斷。
    Ok(status.code().unwrap_or(130))
}
