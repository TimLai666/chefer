//! `chefer version` — 顯示 Chefer 與環境版本資訊（保留原表格 UI）。

use anyhow::Result;
use comfy_table::{Cell, Color};
use owo_colors::OwoColorize;

use crate::ui;
use crate::{APPCIPE_SPEC_VERSION, HOST_TARGET};

pub fn cmd_version() -> Result<()> {
    // 這些環境變數由 build.rs 注入
    let chefer_ver = env!("CARGO_PKG_VERSION");
    let build_time = env!("BUILD_TIME");

    let mut t = ui::kv_table(32, 68);
    t.add_row(vec![
        Cell::new("Chefer").fg(Color::Cyan),
        Cell::new(chefer_ver).fg(Color::Yellow),
    ]);
    t.add_row(vec![
        Cell::new("AppCipe Spec (Latest Supported)").fg(Color::Cyan),
        Cell::new(APPCIPE_SPEC_VERSION),
    ]);
    t.add_row(vec![
        Cell::new("Target").fg(Color::Cyan),
        Cell::new(HOST_TARGET),
    ]);
    t.add_row(vec![
        Cell::new("Build Time (UTC)").fg(Color::Cyan),
        Cell::new(build_time),
    ]);

    println!("\n{}", "▎Version Information".bold());
    println!("{t}\n");
    Ok(())
}
