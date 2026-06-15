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
    /// Generate an appcipe.yml template in the given directory (won't overwrite existing files)
    Init {
        /// Target directory; defaults to the current directory
        #[arg(value_name = "DIR")]
        dir: Option<PathBuf>,
    },

    /// Read and validate appcipe.yml, then print a summary in the chosen format
    Check {
        /// Path to appcipe.yml or its directory; defaults to appcipe.yml
        #[arg(value_name = "PATH")]
        file: Option<String>,

        /// Output format: pretty/json/yaml
        #[arg(long, short, value_enum, default_value_t = PrintFmt::Pretty)]
        format: PrintFmt,
    },

    /// Build: appcipe.yml → bundle → single executable
    Build {
        #[command(flatten)]
        opts: BuildOpts,

        /// Target triple (repeatable; defaults to the host target)
        #[arg(long = "target", value_name = "TRIPLE")]
        targets: Vec<String>,

        /// Only run checks and print a pre-build summary; don't actually build
        #[arg(long)]
        dry_run: bool,
    },

    /// Build the single executable for the host target, then run it directly (stdio passthrough, propagates exit code)
    Run {
        #[command(flatten)]
        opts: BuildOpts,
    },

    /// Inspect a Chefer single executable's footer and embedded manifest summary (no run, no extraction)
    Inspect {
        /// Path to the Chefer single executable
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Show Chefer and environment version information
    Version,

    /// Auto-update to the latest version (no cargo required)
    Upgrade {
        #[arg(long, default_value = "stable")]
        channel: String,

        #[arg(long)]
        to: Option<String>,

        #[arg(long, help = "Only check for updates, do not perform upgrade")]
        check_only: bool,
    },

    /// Remove chefer itself (CLI executable + kit); won't touch your packed apps or their data
    #[command(name = "selfrm")]
    Selfrm {
        /// Remove without prompting for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// build / run 共用的參數（run 固定使用本機 target，故 --target 不在此）。
#[derive(Args, Debug, Clone)]
pub(crate) struct BuildOpts {
    /// Path to appcipe.yml or its directory; defaults to appcipe.yml
    #[arg(value_name = "PATH")]
    pub file: Option<String>,

    /// Output root directory (both the bundle and the single file go under <out>/<name>/)
    #[arg(long, default_value = "dist", value_name = "DIR")]
    pub out: PathBuf,

    /// Additional kit search directory (highest priority; repeatable)
    #[arg(long = "kit-dir", value_name = "DIR")]
    pub kit_dirs: Vec<PathBuf>,

    /// zstd compression level (1..=22; defaults to 3)
    #[arg(long, default_value_t = 3, value_name = "N", value_parser = clap::value_parser!(i32).range(1..=22))]
    pub zstd_level: i32,

    /// Don't embed the original appcipe.yml back into the bundle
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
        Cmd::Selfrm { yes } => commands::selfrm::cmd_selfrm(yes),
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
