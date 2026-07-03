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
    about = "Chefer guest agent: assembles the rootfs and runs bundle services inside a Linux environment",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a bundle: assemble each service's rootfs, then start and monitor them in depends_on order
    Run {
        /// Bundle directory (containing manifest.json)
        #[arg(long)]
        bundle: PathBuf,
        /// App data directory (root for persist data and the default rootfs cache)
        #[arg(long)]
        data: PathBuf,
        /// Rootfs cache directory (default <data>/.rootfs-cache)
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Keep the assembled rootfs after services exit (for reuse on the next launch)
        #[arg(long)]
        keep_rootfs: bool,
        /// VM backend (wsl2/vz) only: set up an eth0→loopback bridge for UDP ports inside the VM
        /// (because wslrelay/VZ NAT does not forward UDP). The native Linux backend must not set this flag.
        #[arg(long)]
        udp_bridge: bool,
        /// Native Linux / WSL2 backend only: allow opt-in per-service GPU passthrough (bind host GPU
        /// device nodes + WSL2 driver libs into the container). The WHP/vz VM backends must not set
        /// this flag — a bare micro-VM cannot expose a host GPU.
        #[arg(long)]
        gpu_host: bool,
    },
    /// Print this machine's (the VM's) primary outward-facing IPv4 for the host backend to build a UDP relay; exits non-zero if none
    Vmip,
    /// Assemble only a single service's rootfs into a given directory (for debugging)
    AssembleRootfs {
        /// Bundle directory (containing manifest.json)
        #[arg(long)]
        bundle: PathBuf,
        /// Service name
        #[arg(long)]
        service: String,
        /// Output directory
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
            udp_bridge,
            gpu_host,
        } => {
            let cfg = guest_agent::RunConfig {
                bundle_dir: bundle,
                data_dir: data,
                cache_dir: cache,
                keep_rootfs,
                udp_bridge,
                gpu_host,
            };
            match guest_agent::run_bundle(&cfg) {
                // exit code 透傳（clamp 至 u8 範圍；>255 取低位元組慣例）
                Ok(code) => ExitCode::from((code & 0xff) as u8),
                Err(e) => {
                    eprintln!("guest-agent failed: {e:#}");
                    ExitCode::from(1)
                }
            }
        }
        Cmd::Vmip => match guest_agent::udp_bridge::detect_primary_ipv4() {
            Some(ip) => {
                println!("{ip}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("guest-agent vmip: no outward-facing IPv4 found (loopback only?)");
                ExitCode::from(1)
            }
        },
        Cmd::AssembleRootfs {
            bundle,
            service,
            out,
        } => match assemble_rootfs_cmd(&bundle, &service, &out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("guest-agent assemble-rootfs failed: {e:#}");
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
                "no service `{service}` in the bundle; available services: {}",
                manifest
                    .services
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        guest_agent::rootfs::assemble_rootfs_at(bundle, svc, out)?;
        println!(
            "assembled the rootfs for service `{service}` at {}",
            out.display()
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (bundle, service, out);
        anyhow::bail!(
            "guest-agent can only run in a Linux environment (current platform: {}); \
             the rootfs must be assembled inside Linux (namespaces / WSL2 / VM)",
            std::env::consts::OS
        )
    }
}
