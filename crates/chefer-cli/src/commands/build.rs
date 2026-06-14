//! `chefer build` — appcipe.yml → pack（bundle）→ 對每個 target assemble 成單一執行檔。
//!
//! 流程（DESIGN.md §6 chefer-cli 節）：
//! 1. `appcipe_normalize::load` → 摘要表格；--dry-run 到此為止。
//! 2. kit 搜尋目錄 = `--kit-dir`（最優先）+ `chefer_bundle::kit::default_kit_dirs()`。
//! 3. targets 預設為本機 target（編譯期 BUILD_TARGET）。
//! 4. require_agents = 任一 target 為 Windows / macOS（DESIGN §2：這些目標必須內嵌 guest-agent）。
//! 5. `chefer_pack::pack` → 對每個 target 找 runtime → `chefer_assembler::assemble`
//!    → 輸出 `<out>/<name>/<name>_<target>[.exe]`，最後列印各產物路徑與大小。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use owo_colors::OwoColorize;

use crate::ui;
use crate::{BuildOpts, HOST_TARGET};

/// 單一 target 的建置產物。
pub struct BuiltArtifact {
    pub target: String,
    pub path: PathBuf,
    /// 輸出檔總大小（bytes）。
    pub size: u64,
}

/// 執行 build；回傳各 target 的產物（dry-run 時為空清單）。
pub fn cmd_build(
    opts: &BuildOpts,
    targets: &[String],
    dry_run: bool,
) -> Result<Vec<BuiltArtifact>> {
    let file = crate::resolve_appcipe_path(opts.file.clone());
    let app = appcipe_normalize::load(Path::new(&file))?;

    println!(
        "{}  {} v{}",
        "🔧 Prepare to build".yellow().bold(),
        app.name.blue().bold(),
        app.app_version.as_deref().unwrap_or(&app.version)
    );
    ui::render_summary_table(&app);

    if dry_run {
        println!("{}", "（dry-run）僅前置檢查完成，未實際打包。".dimmed());
        return Ok(Vec::new());
    }

    // targets 預設為本機 target。
    let targets: Vec<String> = if targets.is_empty() {
        vec![HOST_TARGET.to_string()]
    } else {
        targets.to_vec()
    };

    // kit 搜尋順序：--kit-dir（最優先）→ CHEFER_KIT_DIR → exe旁 kit/ → exe旁。
    let mut kit_dirs: Vec<PathBuf> = opts.kit_dirs.clone();
    kit_dirs.extend(chefer_bundle::kit::default_kit_dirs());

    // Windows / macOS 目標的單檔必須內嵌 guest-agent（DESIGN §2）。
    let require_agents = targets.iter().any(|t| target_requires_agents(t));

    // 1) pack：image tar → bundle（<out>/<name>/bundle）。
    let pack_opts = chefer_pack::PackOptions {
        out_dir: opts.out.clone(),
        clean: true,
        write_original_yml: !opts.no_embed_original,
        kit_dirs: kit_dirs.clone(),
        target_triples: targets.clone(),
        require_agents,
        zstd_level: opts.zstd_level,
        builder_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let pack_res = chefer_pack::pack(&app, &pack_opts).context("打包 bundle 失敗")?;
    println!("📦 Bundle: {}", pack_res.bundle_dir.display());

    // 2) 對每個 target：找 runtime → assemble 單一執行檔。
    let mut artifacts = Vec::with_capacity(targets.len());
    for target in &targets {
        let allow_plain = target == HOST_TARGET;
        let runtime =
            chefer_bundle::kit::find_runtime(&kit_dirs, target, allow_plain).ok_or_else(|| {
                anyhow!(
                    "{}",
                    chefer_bundle::kit::not_found_help(
                        &kit_dirs,
                        &chefer_bundle::kit::runtime_file_name(target),
                    )
                )
            })?;

        let out_path = opts
            .out
            .join(&app.name)
            .join(output_file_name(&app.name, target));
        let report = chefer_assembler::assemble(
            &runtime,
            &pack_res.bundle_dir,
            &out_path,
            &chefer_assembler::AssembleOptions {
                zstd_level: opts.zstd_level,
            },
        )
        .with_context(|| format!("組裝單一執行檔失敗（target: {target}）"))?;

        artifacts.push(BuiltArtifact {
            target: target.clone(),
            path: out_path,
            size: report.out_size,
        });
    }

    // 3) 列印產物路徑與大小。
    println!();
    for a in &artifacts {
        println!(
            "{}  {}  {}  {}",
            "✔ Built".green().bold(),
            a.target.cyan(),
            a.path.display().to_string().blue(),
            format!("({})", ui::human_mb(a.size)).dimmed()
        );
    }
    Ok(artifacts)
}

/// 輸出檔名規則：`<name>_<target>`；Windows target 加 `.exe`。
pub fn output_file_name(name: &str, target: &str) -> String {
    if target.contains("windows") {
        format!("{name}_{target}.exe")
    } else {
        format!("{name}_{target}")
    }
}

/// 是否為「必須內嵌 guest-agent」的目標（Windows / macOS；DESIGN §2）。
pub fn target_requires_agents(target: &str) -> bool {
    target.contains("windows") || target.contains("darwin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_file_name_adds_exe_for_windows() {
        assert_eq!(
            output_file_name("MyApp", "x86_64-pc-windows-msvc"),
            "MyApp_x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            output_file_name("MyApp", "x86_64-unknown-linux-gnu"),
            "MyApp_x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            output_file_name("MyApp", "aarch64-apple-darwin"),
            "MyApp_aarch64-apple-darwin"
        );
    }

    #[test]
    fn agents_required_for_windows_and_macos_targets() {
        assert!(target_requires_agents("x86_64-pc-windows-msvc"));
        assert!(target_requires_agents("aarch64-apple-darwin"));
        assert!(!target_requires_agents("x86_64-unknown-linux-gnu"));
        assert!(!target_requires_agents("aarch64-unknown-linux-musl"));
    }
}
