//! macOS `vz` 後端（Apple Virtualization.framework）。
//!
//! 設計見 docs/DESIGN.md §6「macOS（vz）」：開一台輕量 Linux micro-VM，
//! 以 bundle 內附的 appliance（`vm/chefer-vmlinuz-<arch>` + `chefer-initramfs-<arch>`）
//! 開機，virtiofs 共享 bundle/data，guest 內 initramfs 執行 guest-agent。
//!
//! 狀態：契約與跨平台純邏輯（[`crate::vz_util`]）已完成並測試；實際的
//! Virtualization.framework 開機 shim 與 Linux appliance 尚待在實體 Mac /
//! Linux+QEMU 上實作與驗證。**在驗證通過前 `availability()` 一律回報
//! `Unavailable`，絕不偽稱可執行。**

use anyhow::Result;

use crate::vz_util;
use crate::{AppRunContext, Availability, ExecBackend};

/// macOS Virtualization.framework 後端。
pub struct VzBackend;

impl ExecBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    fn availability(&self) -> Availability {
        // VZ 開機 shim 尚未實作/驗證（需實體 Mac）。誠實回報不可用，
        // 並指出設計位置與替代平台，而非假裝可執行。
        Availability::Unavailable(
            "macOS 執行後端（Virtualization.framework）設計已定（見 docs/DESIGN.md §6），\
             但 VM 開機 shim 與 Linux appliance 尚未實作/驗證；\
             目前請於 Linux 或 Windows 執行此單檔。"
                .to_string(),
        )
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        // 即使尚未能開機，也先跑通「定位 appliance + 計算資源」這條純邏輯，
        // 讓錯誤訊息帶上實際診斷（哪個 arch、appliance 是否內嵌）。
        let host_arch = std::env::consts::ARCH;
        let arch = vz_util::host_guest_arch(host_arch)
            .ok_or_else(|| anyhow::anyhow!("不支援的 host 架構：{host_arch}"))?;

        match vz_util::appliance_in_bundle(ctx.bundle_dir, arch) {
            Some(ap) => anyhow::bail!(
                "macOS 執行後端尚未實作（已找到內嵌 appliance：{}、{}）。\
                 VM 開機 shim 待在實體 Mac 上完成；目前請於 Linux 或 Windows 執行。",
                ap.kernel.display(),
                ap.initramfs.display()
            ),
            None => anyhow::bail!(
                "此單檔未內嵌 macOS micro-VM appliance（缺 vm/chefer-vmlinuz-{arch} 或 \
                 chefer-initramfs-{arch}），且 macOS 執行後端尚未實作；\
                 目前請於 Linux 或 Windows 執行。"
            ),
        }
    }
}
