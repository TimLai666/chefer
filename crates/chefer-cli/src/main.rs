//! chefer-cli — Chefer 的命令列入口（DESIGN.md §6 chefer-cli 節）。
//!
//! 子命令：init / check / build / run / inspect / version / upgrade。
//! 各命令實作位於 `commands/` 下（一檔一命令）；表格 UI 輔助集中在 `ui.rs`。

mod commands;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

/// GitHub repo 擁有者（upgrade 用；統一在此宣告，勿在命令內重複）。
pub(crate) const REPO_OWNER: &str = "TimLai666";
/// GitHub repo 名稱（upgrade 用）。
pub(crate) const REPO_NAME: &str = "chefer";
/// 發佈的二進位檔名（upgrade 用）。
pub(crate) const BIN_NAME: &str = "chefer";
/// 目前支援的 appcipe 規格版本。
pub(crate) const APPCIPE_SPEC_VERSION: &str = "0.1";
/// 編譯期注入的 host target triple（由 build.rs 注入 BUILD_TARGET）。
pub(crate) const HOST_TARGET: &str = env!("BUILD_TARGET");

#[derive(Parser, Debug)]
#[command(
    name = "chefer",
    version,
    about = "Chefer — Cook Your Containers into Delicious Apps"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 在指定目錄產生 appcipe.yml 範本（不覆蓋既有檔案）
    Init {
        /// 目標目錄；預設為目前目錄
        #[arg(value_name = "DIR")]
        dir: Option<PathBuf>,
    },

    /// 讀取並驗證 appcipe.yml，依格式輸出摘要
    Check {
        /// appcipe.yml 路徑或所在目錄，預設 appcipe.yml
        #[arg(value_name = "PATH")]
        file: Option<String>,

        /// 輸出格式：pretty/json/yaml
        #[arg(long, short, value_enum, default_value_t = PrintFmt::Pretty)]
        format: PrintFmt,
    },

    /// 打包：appcipe.yml → bundle → 單一執行檔
    Build {
        #[command(flatten)]
        opts: BuildOpts,

        /// 目標 target triple（可重複；預設為本機 target）
        #[arg(long = "target", value_name = "TRIPLE")]
        targets: Vec<String>,

        /// 只做檢查與前置摘要，不實際打包
        #[arg(long)]
        dry_run: bool,
    },

    /// 建置本機 target 的單一執行檔後直接執行（stdio 直通、透傳 exit code）
    Run {
        #[command(flatten)]
        opts: BuildOpts,
    },

    /// 檢視 Chefer 單一執行檔的 footer 與內嵌 manifest 摘要（不執行、不解壓）
    Inspect {
        /// Chefer 單一執行檔路徑
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// 顯示 Chefer 與環境版本資訊
    Version,

    /// 自動更新到最新版（不依賴 cargo）
    Upgrade {
        #[arg(long, default_value = "stable")]
        channel: String,

        #[arg(long)]
        to: Option<String>,

        #[arg(long, help = "Only check for updates, do not perform upgrade")]
        check_only: bool,
    },
}

/// build / run 共用的參數（run 固定使用本機 target，故 --target 不在此）。
#[derive(Args, Debug, Clone)]
pub(crate) struct BuildOpts {
    /// appcipe.yml 路徑或所在目錄，預設 appcipe.yml
    #[arg(value_name = "PATH")]
    pub file: Option<String>,

    /// 輸出根目錄（bundle 與單檔都會放在 <out>/<name>/ 之下）
    #[arg(long, default_value = "dist", value_name = "DIR")]
    pub out: PathBuf,

    /// 額外的 kit 搜尋目錄（最優先；可重複指定）
    #[arg(long = "kit-dir", value_name = "DIR")]
    pub kit_dirs: Vec<PathBuf>,

    /// zstd 壓縮等級（1..=22；預設 3）
    #[arg(long, default_value_t = 3, value_name = "N")]
    pub zstd_level: i32,

    /// 不在 bundle 內回寫原始 appcipe.yml
    #[arg(long)]
    pub no_embed_original: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum PrintFmt {
    Pretty,
    Json,
    Yaml,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { dir } => commands::init::cmd_init(dir.as_deref()),
        Cmd::Check { file, format } => {
            let file = resolve_appcipe_path(file);
            commands::check::cmd_check(&file, format)
        }
        Cmd::Build {
            opts,
            targets,
            dry_run,
        } => {
            commands::build::cmd_build(&opts, &targets, dry_run)?;
            Ok(())
        }
        Cmd::Run { opts } => {
            let code = commands::run::cmd_run(&opts)?;
            // 透傳被執行應用的 exit code
            std::process::exit(code);
        }
        Cmd::Inspect { file } => commands::inspect::cmd_inspect(&file),
        Cmd::Version => commands::version::cmd_version(),
        Cmd::Upgrade {
            channel,
            to,
            check_only,
        } => commands::upgrade::cmd_upgrade(&channel, to.as_deref(), check_only),
    }
}

/// 根據 file 參數自動尋找 appcipe.yml：
/// - 未給 → 目前目錄的 appcipe.yml
/// - 給目錄（或 "."）→ 該目錄下的 appcipe.yml
/// - 給檔案路徑 → 原樣使用
pub(crate) fn resolve_appcipe_path(file: Option<String>) -> String {
    use std::path::Path;
    match file {
        None => "appcipe.yml".to_string(),
        Some(ref f) if f == "." || Path::new(f).is_dir() => {
            let dir = if f == "." {
                PathBuf::from(".")
            } else {
                PathBuf::from(f)
            };
            dir.join("appcipe.yml").to_string_lossy().to_string()
        }
        Some(f) => f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_to_appcipe_yml() {
        assert_eq!(resolve_appcipe_path(None), "appcipe.yml");
    }

    #[test]
    fn resolve_dot_appends_file_name() {
        let p = resolve_appcipe_path(Some(".".to_string()));
        assert!(p.ends_with("appcipe.yml"), "{p}");
    }

    #[test]
    fn resolve_directory_appends_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = resolve_appcipe_path(Some(dir.path().to_string_lossy().to_string()));
        assert!(p.ends_with("appcipe.yml"), "{p}");
        assert!(
            p.starts_with(&dir.path().to_string_lossy().to_string()),
            "{p}"
        );
    }

    #[test]
    fn resolve_plain_file_is_kept() {
        assert_eq!(
            resolve_appcipe_path(Some("foo/bar.yml".to_string())),
            "foo/bar.yml"
        );
    }
}
