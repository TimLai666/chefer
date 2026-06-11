//! `chefer upgrade` — 從 GitHub Releases 自動更新（不依賴 cargo）。
//!
//! repo 常數統一使用 crate 根的 `REPO_OWNER` / `REPO_NAME` / `BIN_NAME`，
//! 不在此重複宣告。

use anyhow::Result;
use owo_colors::OwoColorize;
use self_update::cargo_crate_version;

use crate::{BIN_NAME, REPO_NAME, REPO_OWNER};

pub fn cmd_upgrade(channel: &str, to: Option<&str>, check_only: bool) -> Result<()> {
    use self_update::Status;
    use self_update::backends::github::Update;

    let mut builder = Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .show_download_progress(true)
        .current_version(cargo_crate_version!());

    if let Some(ver) = to {
        builder.target_version_tag(ver);
    }
    if channel != "stable" {
        builder.target_version_tag(channel);
    }

    let updater = builder.build()?;

    if check_only {
        // 只查詢最新版本，不實際下載與替換執行檔。
        let current = cargo_crate_version!();
        let latest = updater.get_latest_release()?;
        let newer = self_update::version::bump_is_greater(current, &latest.version)
            .unwrap_or(latest.version != current);
        if newer {
            println!(
                "{}  {} → {} (update available)",
                "Update available".yellow().bold(),
                current,
                latest.version
            );
        } else {
            println!("{}  {}", "Already up to date".green().bold(), current);
        }
        return Ok(());
    }

    match updater.update()? {
        Status::UpToDate(ver) => {
            println!("{}  {}", "Already up to date".green().bold(), ver);
        }
        Status::Updated(ver) => {
            println!(
                "{}  {} → {}",
                "✔ Updated".green().bold(),
                cargo_crate_version!(),
                ver
            );
        }
    }

    Ok(())
}
