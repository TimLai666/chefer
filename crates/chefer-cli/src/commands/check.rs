//! `chefer check [path] [--format pretty|json|yaml]` — 驗證 appcipe.yml 並輸出摘要。
//!
//! 一律走 `appcipe_normalize::load`（讀檔 → 解析 → 路徑正規化 → 驗證）；
//! check 不驗證 mounts host 路徑是否存在（那是 pack 階段的責任）。

use std::path::Path;

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::PrintFmt;
use crate::ui;

pub fn cmd_check(file: &str, fmt: PrintFmt) -> Result<()> {
    let app = appcipe_normalize::load(Path::new(file))?;
    println!(
        "{}  {} v{}",
        "✔ Verified".green().bold(),
        app.name.blue().bold(),
        app.app_version.as_deref().unwrap_or(&app.version)
    );
    match fmt {
        PrintFmt::Pretty => {
            ui::render_summary_table(&app);
        }
        PrintFmt::Json => {
            let s = serde_json::to_string_pretty(&app)?;
            println!("{s}");
        }
        PrintFmt::Yaml => {
            let s = serde_yaml::to_string(&app)?;
            println!("{s}");
        }
    }
    Ok(())
}
