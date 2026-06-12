//! guest-agent CLI：在 Linux 環境內執行 chefer bundle。
//!
//! 子命令：
//! - `run`：組裝 rootfs、依依賴順序啟動並監控所有服務。
//! - `assemble-rootfs`：只組裝單一服務的 rootfs 到指定目錄（除錯用）。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "guest-agent",
    about = "Chefer guest agent：於 Linux 環境內組裝 rootfs 並執行 bundle 服務",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 執行 bundle：組裝各服務 rootfs、依 depends_on 順序啟動並監控
    Run {
        /// bundle 目錄（內含 manifest.json）
        #[arg(long)]
        bundle: PathBuf,
        /// app 資料目錄（persist 資料與預設 rootfs 快取的根）
        #[arg(long)]
        data: PathBuf,
        /// rootfs 快取目錄（預設 <data>/.rootfs-cache）
        #[arg(long)]
        cache: Option<PathBuf>,
        /// 服務結束後保留已組裝的 rootfs（供下次啟動重用）
        #[arg(long)]
        keep_rootfs: bool,
    },
    /// 只組裝單一服務的 rootfs 到指定目錄（除錯用）
    AssembleRootfs {
        /// bundle 目錄（內含 manifest.json）
        #[arg(long)]
        bundle: PathBuf,
        /// 服務名稱
        #[arg(long)]
        service: String,
        /// 輸出目錄
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    // busybox 風格 applet：以 mount/umount 名稱（symlink）被呼叫時，
    // 執行對應 applet——WSL init 啟動 distro 時需要 distro 內有可用的 mount。
    if let Some(code) = guest_agent::applets::maybe_run_applet() {
        return ExitCode::from((code & 0xff) as u8);
    }

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            bundle,
            data,
            cache,
            keep_rootfs,
        } => {
            let cfg = guest_agent::RunConfig {
                bundle_dir: bundle,
                data_dir: data,
                cache_dir: cache,
                keep_rootfs,
            };
            match guest_agent::run_bundle(&cfg) {
                // exit code 透傳（clamp 至 u8 範圍；>255 取低位元組慣例）
                Ok(code) => ExitCode::from((code & 0xff) as u8),
                Err(e) => {
                    eprintln!("guest-agent 執行失敗：{e:#}");
                    ExitCode::from(1)
                }
            }
        }
        Cmd::AssembleRootfs {
            bundle,
            service,
            out,
        } => match assemble_rootfs_cmd(&bundle, &service, &out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("guest-agent assemble-rootfs 失敗：{e:#}");
                ExitCode::from(1)
            }
        },
    }
}

/// `assemble-rootfs` 子命令：讀 manifest、找服務、解所有層到 --out。
fn assemble_rootfs_cmd(
    bundle: &std::path::Path,
    service: &str,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let manifest = guest_agent::load_manifest(bundle)?;
        let svc = manifest.service(service).ok_or_else(|| {
            anyhow::anyhow!(
                "bundle 內沒有服務 `{service}`；現有服務：{}",
                manifest
                    .services
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        guest_agent::rootfs::assemble_rootfs_at(bundle, svc, out)?;
        println!("已組裝服務 `{service}` 的 rootfs 至 {}", out.display());
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (bundle, service, out);
        anyhow::bail!(
            "guest-agent 僅能於 Linux 環境執行（目前平台：{}）；\
             rootfs 需在 Linux（namespaces / WSL2 / VM）內組裝",
            std::env::consts::OS
        )
    }
}
