//! guest-agent — 在 Linux 環境（namespaces / WSL2 distro / 未來 VM）內
//! 由 bundle 組裝 rootfs 並啟動、監控各服務。
//!
//! 對應 docs/DESIGN.md §6「guest-agent」節：
//! - lib 入口：[`RunConfig`] + [`run_bundle`]。
//! - rootfs 組裝：[`rootfs`]（層解壓、whiteout、快取）。
//! - whiteout 檔名解析：[`whiteout`]（純函式，跨平台可測）。
//! - 服務生命週期：`supervisor`（僅 Linux）。
//! - namespaces + exec：`exec`（僅 Linux）。

pub mod rootfs;
pub mod whiteout;

#[cfg(target_os = "linux")]
pub mod exec;
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
}

/// 讀取 bundle 內的 manifest.json（含可行動的錯誤訊息）。
pub fn load_manifest(bundle_dir: &Path) -> Result<chefer_bundle::Manifest> {
    let path = chefer_bundle::layout::manifest_path(bundle_dir);
    if !path.exists() {
        anyhow::bail!(
            "找不到 bundle manifest：{}；請確認 --bundle 指向解壓後的 bundle 目錄（內含 manifest.json），\
             或先以 `chefer build` 重新產生 bundle",
            path.display()
        );
    }
    chefer_bundle::Manifest::load(&path)
        .with_context(|| format!("載入 bundle manifest 失敗：{}", path.display()))
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
            "guest-agent 僅能於 Linux 環境執行（目前平台：{}）；\
             請改用 chefer 產出的單一執行檔，由其在 Windows 透過 WSL2、在 macOS 透過 VM 後端啟動",
            std::env::consts::OS
        )
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::collections::BTreeMap;
    use std::path::Path;

    use anyhow::{Result, bail};

    use crate::{RunConfig, load_manifest, rootfs, supervisor};

    pub fn run_bundle_linux(cfg: &RunConfig) -> Result<i32> {
        // 先安裝訊號處理：rootfs 組裝期間收到 SIGTERM/SIGINT 也走相同終止流程
        supervisor::install_signal_handlers()?;

        let manifest = load_manifest(&cfg.bundle_dir)?;
        let order = chefer_bundle::topo_sort(&manifest.services)?;

        validate_services(&order)?;

        // 組裝（或重用快取的）各服務 rootfs
        let cache_root = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| rootfs::default_cache_root(&cfg.data_dir));
        let mut rootfs_map: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
        for svc in &order {
            let dir = rootfs::assemble_service_rootfs(&cfg.bundle_dir, svc, &cache_root)?;
            rootfs_map.insert(svc.name.clone(), dir);
        }

        // 依拓撲順序啟動並監控
        let code = supervisor::run_services(&order, &rootfs_map, &cfg.data_dir)?;

        // 未要求保留時，盡力清掉 rootfs 快取（失敗僅警告，不影響 exit code）
        if !cfg.keep_rootfs {
            for (name, dir) in &rootfs_map {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    eprintln!(
                        "[guest-agent] 警告：清理服務 `{name}` 的 rootfs 失敗（{}）：{e}；\
                         可手動刪除該目錄",
                        dir.display()
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
                    "service `{}` 的平台 `{}` 尚未支援執行（目前支援 linux/amd64、linux/arm64）；\
                     請改用 Linux 平台的 image 重新打包",
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
                "同時有多個服務宣告 terminal 介面（{}）；terminal/both 介面最多只能有一個服務，\
                 請調整 appcipe.yml 的 interface 設定後重新打包",
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
                "下列掛載的 host 路徑不存在，請先建立（或修正 appcipe.yml 的 mounts 後重新打包）：\n{}",
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
        };
        let err = run_bundle(&cfg).unwrap_err();
        assert!(format!("{err}").contains("僅能於 Linux"));
    }
}
