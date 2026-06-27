//! guest-agent — 在 Linux 環境（namespaces / WSL2 distro / 未來 VM）內
//! 由 bundle 組裝 rootfs 並啟動、監控各服務。
//!
//! 對應 docs/DESIGN.md §6「guest-agent」節：
//! - lib 入口：[`RunConfig`] + [`run_bundle`]。
//! - rootfs 組裝：[`rootfs`]（層解壓、whiteout、快取）。
//! - whiteout 檔名解析：[`whiteout`]（純函式，跨平台可測）。
//! - 服務生命週期：`supervisor`（僅 Linux）。
//! - namespaces + exec：`exec`（僅 Linux）。

pub mod applets;
pub mod rootfs;
pub mod udp_bridge;
pub mod whiteout;

#[cfg(target_os = "linux")]
pub mod exec;
#[cfg(target_os = "linux")]
pub mod health;
#[cfg(target_os = "linux")]
pub mod netns;
#[cfg(target_os = "linux")]
pub mod supervisor;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 執行 bundle 所需的設定。
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// bundle 目錄（內含 manifest.json 與 services/<svc>/layers/）。
    pub bundle_dir: PathBuf,
    /// app 資料目錄（persist 資料與預設 rootfs 快取的根）。
    pub data_dir: PathBuf,
    /// rootfs 快取目錄；None 時使用 `<data_dir>/.rootfs-cache`。
    pub cache_dir: Option<PathBuf>,
    /// 服務結束後是否保留已組裝的 rootfs（供下次啟動重用）。
    pub keep_rootfs: bool,
    /// VM 後端（wsl2/vz）專用：在 VM 內補起 UDP 埠的 `<vm_ip>:guest → 127.0.0.1:guest`
    /// 橋接（因 wslrelay/VZ NAT 不轉 UDP）。原生 Linux namespaces 後端必須為 false
    /// ——共享 netns 下直接以 loopback 生效，且綁 LAN IP 會把服務暴露到區網。
    pub udp_bridge: bool,
}

/// 讀取 bundle 內的 manifest.json（含可行動的錯誤訊息）。
pub fn load_manifest(bundle_dir: &Path) -> Result<chefer_bundle::Manifest> {
    let path = chefer_bundle::layout::manifest_path(bundle_dir);
    if !path.exists() {
        anyhow::bail!(
            "bundle manifest not found: {}; make sure --bundle points to an extracted bundle directory (containing manifest.json), \
             or regenerate the bundle with `chefer build`",
            path.display()
        );
    }
    chefer_bundle::Manifest::load(&path)
        .with_context(|| format!("failed to load bundle manifest: {}", path.display()))
}

/// 執行整個 bundle：組裝各服務 rootfs、依依賴順序啟動並監控，回傳 app 整體 exit code。
///
/// 非 Linux 平台回傳明確錯誤（guest-agent 僅能於 Linux 環境執行）。
pub fn run_bundle(cfg: &RunConfig) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::run_bundle_linux(cfg)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        anyhow::bail!(
            "guest-agent can only run in a Linux environment (current platform: {}); \
             use the single-file executable produced by chefer instead, which launches it via WSL2 on Windows and via the VM backend on macOS",
            std::env::consts::OS
        )
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::collections::BTreeMap;
    use std::path::Path;

    use anyhow::{Context, Result, bail};
    use chefer_bundle::NetworkMode;

    use crate::exec::{OverlayDirs, ServiceRootfs};

    use crate::{RunConfig, load_manifest, netns, rootfs, supervisor};

    pub fn run_bundle_linux(cfg: &RunConfig) -> Result<i32> {
        // 先安裝訊號處理：rootfs 組裝期間收到 SIGTERM/SIGINT 也走相同終止流程
        supervisor::install_signal_handlers()?;

        let manifest = load_manifest(&cfg.bundle_dir)?;
        let order = chefer_bundle::topo_sort(&manifest.services)?;

        validate_services(&order)?;

        // 網路模式：shared 沿用共享 netns（現況）；internal/bridge 建立 per-app netns。
        // bridge 另起 pasta 提供出網（找不到 pasta 則退化為 internal，由 setup_app_netns 印訊息）。
        let app_net = match manifest.app.network {
            NetworkMode::Shared => None,
            mode @ (NetworkMode::Internal | NetworkMode::Bridge) => {
                let rootless = !nix::unistd::geteuid().is_root();
                let bridge = matches!(mode, NetworkMode::Bridge);
                // bundle 內嵌 pasta 的路徑（agents/pasta-<arch>），供 bridge 出網。
                let pasta_hint = chefer_bundle::layout::agents_dir(&cfg.bundle_dir)
                    .join(chefer_bundle::layout::pasta_name(std::env::consts::ARCH));
                let net = netns::setup_app_netns(rootless, bridge, Some(pasta_hint))?;
                eprintln!(
                    "[guest-agent] network mode {}: created a per-app network namespace (lo only)",
                    if bridge { "bridge" } else { "internal" }
                );
                Some(net)
            }
        };

        // VM 後端 + app-netns 模式：補起 eth0→loopback 的埠橋接（背景執行緒，等服務先綁好埠的寬限期）。
        // host→guest 流量（WHP smoltcp / vz NAT）落在 eth0，但 app-netns 的 inbound relay 綁 loopback，
        // 故 TCP 與 UDP 都需此橋接。**shared 模式不需也不可橋接**：服務直接綁 root netns（通常 0.0.0.0、
        // 已涵蓋 eth0），橋接會與其搶埠（如 QEMU E2E 的 web 綁 0.0.0.0:8080）。原生 Linux
        // （udp_bridge=false）亦不橋接。
        if cfg.udp_bridge && app_net.is_some() {
            crate::udp_bridge::start_vm_tcp_bridges(&manifest);
            crate::udp_bridge::start_vm_udp_bridges(&manifest);
        }

        // 組裝（或重用快取的）各服務 rootfs
        let cache_root = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| rootfs::default_cache_root(&cfg.data_dir));
        // root 後端可用 overlayfs：各層解到獨立唯讀 lowerdir（共用、去重）、每服務一個可寫
        // upper/work 疊上去——免合併複製、掛載瞬間完成。rootless / 不支援 overlay → 維持合併。
        let use_overlay = rootfs::overlay_supported();
        if use_overlay {
            eprintln!("[guest-agent] rootfs via overlayfs (root backend; lazy, shared layers)");
        }
        // overlay 的每次執行可寫層根（以 pid 區隔；結束後清除）。
        // **放系統 temp（通常 /tmp）而非 data_dir**：overlay 的 upperdir 對檔案系統有要求
        // （d_type、xattr 等），而 data_dir 在 VM 後端常是 virtiofs（缺這些 → mount EINVAL
        // 「upper fs missing required features」）。temp 與 overlay_supported() 的實測掛載同一處，
        // 故那個 selftest 正好驗證了這裡能不能當 upper。
        let overlay_run_root = std::env::temp_dir()
            .join("chefer-overlay")
            .join(std::process::id().to_string());

        let mut rootfs_map: BTreeMap<String, ServiceRootfs> = BTreeMap::new();
        // 非 overlay：持有各服務 rootfs 的共享租約直到 run 結束，阻止並行 instance 在使用中刪除。
        let mut leases: Vec<(String, rootfs::RootfsLease)> = Vec::new();
        for svc in &order {
            if use_overlay {
                let lowers = rootfs::assemble_overlay_layers(&cfg.bundle_dir, svc, &cache_root)?;
                let svc_root = overlay_run_root.join(&svc.name);
                let upper = svc_root.join("upper");
                let work = svc_root.join("work");
                let merged = svc_root.join("merged");
                for d in [&upper, &work, &merged] {
                    std::fs::create_dir_all(d).with_context(|| {
                        format!("failed to create overlay dir: {}", d.display())
                    })?;
                }
                rootfs_map.insert(
                    svc.name.clone(),
                    ServiceRootfs {
                        root: merged,
                        overlay: Some(OverlayDirs {
                            lowers,
                            upper,
                            work,
                        }),
                    },
                );
            } else {
                let lease = rootfs::assemble_service_rootfs(&cfg.bundle_dir, svc, &cache_root)?;
                rootfs_map.insert(
                    svc.name.clone(),
                    ServiceRootfs {
                        root: lease.path().to_path_buf(),
                        overlay: None,
                    },
                );
                leases.push((svc.name.clone(), lease));
            }
        }

        // 依拓撲順序啟動並監控（internal/bridge 模式下各服務 setns 進 app netns，
        // 並為宣告的 port 起跨 netns inbound relay）。
        let run_result = supervisor::run_services(
            &order,
            &rootfs_map,
            &cfg.data_dir,
            app_net.as_ref(),
            &manifest,
        );

        // app 結束：收掉 netns holder（釋放 app netns）。
        if let Some(net) = app_net {
            net.shutdown();
        }
        let code = run_result?;

        // overlay 模式：清掉本次執行的可寫層（upper/work/merged 為 ephemeral；lowerdir 各層
        // 內容定址、持久共用，不在此刪）。即使 keep_rootfs 也清——這些只是本次執行的暫存寫入層。
        if use_overlay {
            let _ = std::fs::remove_dir_all(&overlay_run_root);
        }

        // 非 overlay：未要求保留時，嘗試清掉合併 rootfs 快取——只有在無其他 instance 使用時
        // （能升級為獨佔鎖）才真正刪除，否則安全跳過。
        if !cfg.keep_rootfs {
            for (name, lease) in leases {
                if !lease.cleanup() {
                    eprintln!(
                        "[guest-agent] the rootfs for service `{name}` is still in use by another running instance, skipping cleanup"
                    );
                }
            }
        }
        Ok(code)
    }

    /// 啟動前整體驗證：平台支援、terminal 服務數量、bind mounts 的 host 路徑存在。
    fn validate_services(order: &[&chefer_bundle::ServiceEntry]) -> Result<()> {
        // 平台：目前僅支援 linux/amd64、linux/arm64
        for svc in order {
            if chefer_bundle::layout::platform_to_arch(&svc.platform).is_none() {
                bail!(
                    "service `{}` platform `{}` is not yet supported for execution (currently supported: linux/amd64, linux/arm64); \
                     repack using a Linux-platform image",
                    svc.name,
                    svc.platform
                );
            }
        }

        // interface_mode 含 terminal 的服務最多一個（驗證期應已保證，此處再防衛一次）
        let terminals: Vec<&str> = order
            .iter()
            .filter(|s| s.interface_mode.wants_terminal())
            .map(|s| s.name.as_str())
            .collect();
        if terminals.len() > 1 {
            bail!(
                "multiple services declare a terminal interface ({}); at most one service may use a terminal/both interface, \
                 adjust the interface setting in appcipe.yml and repack",
                terminals.join(", ")
            );
        }

        // bind mounts 的 host 路徑必須存在（啟動前整體報錯）
        let mut missing: Vec<String> = Vec::new();
        for svc in order {
            for m in &svc.mounts {
                if !Path::new(&m.host).exists() {
                    missing.push(format!("  service `{}`：{}", svc.name, m.host));
                }
            }
        }
        if !missing.is_empty() {
            bail!(
                "the following mount host paths do not exist, please create them first (or fix the mounts in appcipe.yml and repack):\n{}",
                missing.join("\n")
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_manifest_missing_gives_actionable_error() {
        let dir = std::env::temp_dir().join("guest-agent-test-no-manifest");
        let _ = std::fs::create_dir_all(&dir);
        let err = load_manifest(&dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("manifest.json"),
            "錯誤訊息應指出 manifest.json：{msg}"
        );
        assert!(
            msg.contains("--bundle"),
            "錯誤訊息應提示 --bundle 參數：{msg}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn run_bundle_errors_on_non_linux() {
        let cfg = RunConfig {
            bundle_dir: PathBuf::from("x"),
            data_dir: PathBuf::from("y"),
            cache_dir: None,
            keep_rootfs: false,
            udp_bridge: false,
        };
        let err = run_bundle(&cfg).unwrap_err();
        assert!(format!("{err}").contains("can only run in a Linux"));
    }
}
